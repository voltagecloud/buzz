//! Agents see Buzz images (buzz-app's `app_agent_media` and the agent
//! controller's `media_url`, `media_auth` and `verify_media_hash`): the
//! host fetches one of the agent's community's media blobs as the agent,
//! signing the Blossom authorization with its key, which never leaves.

use super::{Result, bounded, client, reason};
use base64::Engine;
use buzz_agent_controller::{AppAgent, Secret};
use serde_json::{Value, json};

/// The largest image an agent is shown. Claude takes 5 MiB of base64 on
/// its strictest hosts, and base64 is 4 characters per 3 bytes.
pub(crate) const MEDIA_LIMIT: usize = 5 * 1024 * 1024 / 4 * 3;

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `https://host[:port]/media/`, the agent's community's media.
fn media_base(agent: &AppAgent) -> String {
    format!("{}/media/", agent.relay.replacen("wss://", "https://", 1))
}

/// Its community's URL for one media blob, from that URL or its
/// `sha256[.ext]` name. Media elsewhere is refused, so the signature goes
/// nowhere else.
pub(crate) fn media_url(agent: &AppAgent, input: &str) -> Result<String> {
    let base = media_base(agent);
    let name = input.trim();
    let name = name.strip_prefix(base.as_str()).unwrap_or(name);
    let parts: Vec<&str> = name.split('.').collect();
    let ext = |part: &str| {
        !part.is_empty()
            && part.len() <= 8
            && part
                .bytes()
                .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
    };
    let valid = match parts.as_slice() {
        [hash] => hex64(hash),
        [hash, "thumb", "jpg"] => hex64(hash),
        [hash, part] => hex64(hash) && ext(part),
        _ => false,
    };
    if !valid {
        return Err("Agents fetch only their community's media".into());
    }
    Ok(format!("{base}{name}"))
}

/// Confirms a downloaded media body matches the hash in its community URL.
pub(crate) fn verify_media_hash(agent: &AppAgent, url: &str, bytes: &[u8]) -> Result<()> {
    use sha2::{Digest, Sha256};
    let canonical = media_url(agent, url)?;
    // Thumbnails are derived JPEGs, but keep the source blob's name.
    if canonical.ends_with(".thumb.jpg") {
        return Ok(());
    }
    let expected = canonical
        .rsplit('/')
        .next()
        .and_then(|name| name.get(..64))
        .ok_or("Invalid media hash")?;
    if format!("{:x}", Sha256::digest(bytes)) != expected {
        return Err("Media did not match its SHA-256".into());
    }
    Ok(())
}

/// Blossom authorization (24242) to fetch media from its community, valid
/// for a minute.
pub(crate) fn media_auth(agent: &AppAgent, key: &Secret, now: u64) -> Result<Value> {
    use secp256k1::{Keypair, Secp256k1, SecretKey};
    use sha2::{Digest, Sha256};
    if key.pubkey() != agent.pubkey {
        return Err("Agent identity changed".into());
    }
    let tags = vec![
        vec!["t".to_string(), "get".into()],
        vec!["expiration".into(), (now + 60).to_string()],
        vec![
            "server".into(),
            agent.relay.trim_start_matches("wss://").into(),
        ],
    ];
    let content = "Get media";
    let serialized = serde_json::to_vec(&json!([0, agent.pubkey, now, 24242, tags, content]))
        .map_err(|_| "Could not authorize media")?;
    let id: [u8; 32] = Sha256::digest(serialized).into();
    let mut secret: SecretKey = key.hex().parse().map_err(|_| "Invalid agent key")?;
    let mut pair = Keypair::from_secret_key(&Secp256k1::signing_only(), &secret);
    let sig = Secp256k1::signing_only()
        .sign_schnorr_no_aux_rand(&id, &pair)
        .to_string();
    secret.non_secure_erase();
    pair.non_secure_erase();
    Ok(json!({
        "id": id.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        "pubkey": agent.pubkey,
        "created_at": now,
        "kind": 24242,
        "tags": tags,
        "content": content,
        "sig": sig,
    }))
}

/// Downloads one of its community's media blobs (its URL or
/// `sha256[.ext]`) as the agent, base64.
pub(crate) async fn fetch(agent: &AppAgent, key: &Secret, url: &str) -> Result<String> {
    let url = media_url(agent, url)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "System clock is unavailable")?
        .as_secs();
    let auth = serde_json::to_vec(&media_auth(agent, key, now)?)
        .map_err(|_| "Could not authorize media")?;
    let response = client()?
        .get(&url)
        .timeout(std::time::Duration::from_secs(120))
        .header(
            "Authorization",
            format!(
                "Nostr {}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(auth)
            ),
        )
        .header("x-auth-tag", &agent.auth)
        .send()
        .await
        .map_err(|_| "Agent media request failed")?;
    let status = response.status();
    let body = bounded(response, MEDIA_LIMIT).await?;
    if !status.is_success() {
        return Err(format!(
            "The community refused the media: {}",
            reason(status, &body)
        ));
    }
    verify_media_hash(agent, &url, &body)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(key: &Secret) -> AppAgent {
        AppAgent {
            pubkey: key.pubkey().into(),
            relay: "wss://relay.example".into(),
            owner: "a".repeat(64),
            auth: r#"["auth","owner","","sig"]"#.into(),
            agent_type: "p/t".into(),
            name: "Ada".into(),
            picture: None,
            about: None,
            profile: None,
            deleted: false,
        }
    }

    #[test]
    fn fetches_only_its_communitys_media_and_checks_its_hash() {
        let key = Secret::generate().unwrap();
        let agent = agent(&key);
        let hash = "ab".repeat(32);
        let base = "https://relay.example/media/";
        for (input, expected) in [
            (format!("{base}{hash}.png"), format!("{base}{hash}.png")),
            (hash.clone(), format!("{base}{hash}")),
            (
                format!("{hash}.thumb.jpg"),
                format!("{base}{hash}.thumb.jpg"),
            ),
        ] {
            assert_eq!(media_url(&agent, &input).unwrap(), expected);
        }
        for refused in [
            format!("https://elsewhere.example/media/{hash}.png"),
            format!("{base}{hash}.PNG"),
            format!("{base}{hash}.toolongext"),
            format!("{base}../{hash}"),
            "ab".repeat(31),
        ] {
            assert_eq!(
                media_url(&agent, &refused),
                Err("Agents fetch only their community's media".into())
            );
        }
        let bytes = b"image bytes";
        let sha = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(bytes))
        };
        assert!(verify_media_hash(&agent, &format!("{sha}.png"), bytes).is_ok());
        assert_eq!(
            verify_media_hash(&agent, &format!("{hash}.png"), bytes),
            Err("Media did not match its SHA-256".into())
        );
        // A thumbnail keeps its source's name, not its own hash.
        assert!(verify_media_hash(&agent, &format!("{hash}.thumb.jpg"), bytes).is_ok());
    }

    #[test]
    fn signs_a_get_authorization_for_its_community() {
        let key = Secret::generate().unwrap();
        let auth = media_auth(&agent(&key), &key, 1_000).unwrap();
        assert_eq!(auth["kind"], 24242);
        assert_eq!(auth["pubkey"], key.pubkey());
        assert_eq!(auth["content"], "Get media");
        assert_eq!(
            auth["tags"],
            json!([
                ["t", "get"],
                ["expiration", "1060"],
                ["server", "relay.example"]
            ])
        );
        use secp256k1::{Secp256k1, XOnlyPublicKey, schnorr::Signature};
        use sha2::{Digest, Sha256};
        let serialized = serde_json::to_vec(&json!([
            0,
            auth["pubkey"],
            auth["created_at"],
            24242,
            auth["tags"],
            auth["content"]
        ]))
        .unwrap();
        let id: [u8; 32] = Sha256::digest(serialized).into();
        let id_hex: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(auth["id"], id_hex);
        let signature: Signature = auth["sig"].as_str().unwrap().parse().unwrap();
        let author: XOnlyPublicKey = key.pubkey().parse().unwrap();
        assert!(
            Secp256k1::verification_only()
                .verify_schnorr(&signature, &id, &author)
                .is_ok()
        );
    }
}
