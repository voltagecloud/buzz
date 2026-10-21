//! The account the phone receives: `{relayUrl, pubkey, nsec}`, read from
//! the app's identity only for this attempt, and only if it is still the
//! account the user chose.

use buzz_identity::Identity;
use zeroize::Zeroizing;

pub(super) fn prepare(
    identity: Option<&Identity>,
    viewer: &str,
    origin: &str,
) -> Result<Zeroizing<String>, String> {
    let identity = identity.ok_or("Set up your identity first")?;
    if identity.pubkey_hex() != viewer {
        return Err("The active account changed. Reopen Pair mobile.".into());
    }
    payload(identity, viewer, origin)
}

pub(super) fn payload(
    identity: &Identity,
    expected: &str,
    relay: &str,
) -> Result<Zeroizing<String>, String> {
    if identity.pubkey_hex() != expected {
        return Err("The saved Buzz account has changed. Reopen Pair mobile and try again.".into());
    }
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload<'a> {
        relay_url: &'a str,
        pubkey: &'a str,
        nsec: &'a str,
    }
    let secret = identity.nsec();
    serde_json::to_string(&Payload {
        relay_url: relay,
        pubkey: expected,
        nsec: &secret,
    })
    .map(Zeroizing::new)
    .map_err(|_| "Couldn’t prepare your Buzz account.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_pairing::{Keys, ToBech32};

    #[test]
    fn current_identity_is_the_only_payload_source() {
        let identity = Identity::ephemeral();
        let viewer = identity.pubkey_hex();
        assert!(prepare(Some(&identity), &"f".repeat(64), "https://relay.test").is_err());
        let data = prepare(Some(&identity), &viewer, "https://relay.test").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["pubkey"], viewer);
        assert_eq!(
            Keys::parse(parsed["nsec"].as_str().unwrap())
                .unwrap()
                .public_key()
                .to_hex(),
            viewer
        );
        assert_eq!(
            prepare(None, &viewer, "https://relay.test").unwrap_err(),
            "Set up your identity first"
        );
    }

    #[test]
    fn account_pin_is_checked_before_export() {
        let keys = Keys::generate();
        let identity = Identity::from_nsec(&keys.secret_key().to_bech32().unwrap()).unwrap();
        assert!(
            payload(
                &identity,
                &Keys::generate().public_key().to_hex(),
                "https://relay.test"
            )
            .is_err()
        );
        let data = payload(&identity, &keys.public_key().to_hex(), "https://relay.test").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["relayUrl"], "https://relay.test");
        assert_eq!(
            Keys::parse(parsed["nsec"].as_str().unwrap())
                .unwrap()
                .public_key(),
            keys.public_key()
        );
    }
}
