//! Agents2 profile pictures were an addition of ours before buzz-app's
//! identities held a picture, saved in their own file beside the
//! identities. Each saved picture moves into its identity once, as
//! ordinary profile metadata, and the file goes.

use buzz_agent_controller::AppAgents;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// One agent's entry in the old file; what it last published is
/// republished from the identity.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Saved {
    picture: String,
}

/// Moves the pictures saved at `path` into the identities that have none,
/// then removes the file. A file that cannot be read stays, to try again.
pub(crate) fn migrate(path: &Path, agents: &AppAgents) {
    let saved: BTreeMap<String, Saved> = match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(saved) => saved,
            Err(_) => return tracing::warn!("saved agent pictures are malformed"),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => return tracing::warn!("could not read saved agent pictures"),
    };
    for (pubkey, Saved { picture }) in saved {
        let Ok(agent) = agents.get(&pubkey) else {
            continue;
        };
        if picture.is_empty() || agent.picture.is_some() {
            continue;
        }
        if let Err(error) = agents.set_picture(&pubkey, Some(picture)) {
            tracing::warn!(%error, agent = %pubkey, "saved picture was not moved");
        }
    }
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("lock"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_agent_controller::{AppAgents, Credentials, LegacySource, Secret};
    use secp256k1::{Keypair, Secp256k1, SecretKey};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Memory(Mutex<HashMap<String, String>>);

    impl Credentials for Memory {
        fn read_legacy(&self, _: LegacySource, _: &str) -> Result<Secret, String> {
            Err("no legacy".into())
        }
        fn read(&self, id: &str, pubkey: &str) -> Result<Option<Secret>, String> {
            self.0
                .lock()
                .unwrap()
                .get(id)
                .map(|hex| Secret::parse(hex, pubkey))
                .transpose()
        }
        fn add(&self, id: &str, key: &Secret) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(id.into(), key.hex().to_string());
            Ok(())
        }
        fn delete(&self, id: &str, _: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
    }

    /// An owner attestation for `agent`, as the GUI signs it.
    fn attest(agent: &str) -> Vec<String> {
        let owner = Keypair::from_secret_key(
            &Secp256k1::new(),
            &SecretKey::from_byte_array([7; 32]).unwrap(),
        );
        let digest: [u8; 32] = Sha256::digest(format!("nostr:agent-auth:{agent}:")).into();
        let signature = Secp256k1::new().sign_schnorr_no_aux_rand(&digest, &owner);
        vec![
            "auth".into(),
            owner.x_only_public_key().0.to_string(),
            String::new(),
            signature.to_string(),
        ]
    }

    fn create(agents: &AppAgents, credentials: &Memory, name: &str) -> String {
        let auth = attest("x");
        let owner = auth[1].clone();
        let prepared = AppAgents::prepare("wss://relay.example", &owner, "p/t", name).unwrap();
        let auth = attest(prepared.pubkey());
        agents
            .commit(prepared, &auth, credentials)
            .unwrap()
            .0
            .pubkey
    }

    #[test]
    fn saved_pictures_move_into_identities_once() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Memory::default();
        let agents = AppAgents::open(dir.path().join("identities.json"));
        let ada = create(&agents, &credentials, "Ada");
        let eve = create(&agents, &credentials, "Eve");
        agents
            .set_picture(&eve, Some("https://relay.example/media/eve.png".into()))
            .unwrap();
        let path = dir.path().join("pictures.json");
        let saved = serde_json::json!({
            ada.clone(): { "picture": "https://relay.example/media/ada.png",
                "published": { "picture": "https://relay.example/media/ada.png", "createdAt": 5 } },
            eve.clone(): { "picture": "https://relay.example/media/old.png" },
            "c".repeat(64): { "picture": "https://relay.example/media/gone.png" },
        });
        std::fs::write(&path, saved.to_string()).unwrap();
        migrate(&path, &agents);
        assert_eq!(
            agents.get(&ada).unwrap().picture.as_deref(),
            Some("https://relay.example/media/ada.png")
        );
        assert_eq!(
            agents.get(&eve).unwrap().picture.as_deref(),
            Some("https://relay.example/media/eve.png"),
            "an identity's own picture wins"
        );
        assert!(
            !agents.get(&ada).unwrap().profile_is_current(),
            "republished"
        );
        assert!(!path.exists());
        // Without the file nothing changes.
        agents.set_picture(&ada, None).unwrap();
        migrate(&path, &agents);
        assert_eq!(agents.get(&ada).unwrap().picture, None);
    }
}
