//! Public keys as people see them.

use nostr::nips::nip19::ToBech32;
use nostr::prelude::PublicKey;
use std::collections::{HashMap, HashSet};

/// The full `npub1…` of a hex key, or the key itself if it is not one.
pub fn npub(hex: &str) -> String {
    PublicKey::from_hex(hex)
        .ok()
        .and_then(|key| key.to_bech32().ok())
        .unwrap_or_else(|| hex.to_string())
}

/// `formatPublicKey`: `npub…` and the last three characters, for a valid
/// hex key.
pub fn format_public_key(hex: &str) -> Option<String> {
    let npub = PublicKey::from_hex(hex).ok()?.to_bech32().ok()?;
    Some(format!("npub…{}", &npub[npub.len() - 3..]))
}

/// `publicKeyLabels`: the shortest npub suffix, from three, that tells
/// every key apart.
pub(crate) fn public_key_labels<'a>(
    keys: impl IntoIterator<Item = &'a String>,
) -> HashMap<String, String> {
    let encoded: HashMap<String, String> = keys
        .into_iter()
        .map(|key| (key.to_ascii_lowercase(), npub(key)))
        .collect();
    let mut length = 3;
    while length < 58 {
        let suffixes: HashSet<&str> = encoded
            .values()
            .map(|npub| &npub[npub.len().saturating_sub(length)..])
            .collect();
        if suffixes.len() == encoded.len() {
            break;
        }
        length += 1;
    }
    encoded
        .into_iter()
        .map(|(key, npub)| {
            let suffix = npub[npub.len().saturating_sub(length)..].to_owned();
            (key, format!("npub…{suffix}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn keys_read_as_npubs() {
        let key = buzz_identity::Identity::ephemeral().public_key();
        let npub = key.to_bech32().unwrap();
        assert_eq!(super::npub(&key.to_hex()), npub);
        assert_eq!(super::npub("nope"), "nope");
        assert_eq!(
            format_public_key(&key.to_hex()),
            Some(format!("npub…{}", &npub[npub.len() - 3..]))
        );
        assert_eq!(format_public_key("nope"), None);
    }

    #[test]
    fn key_labels_grow_until_distinct() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let labels = public_key_labels([&a, &b]);
        assert!(labels[&a].starts_with("npub…"));
        assert_ne!(labels[&a], labels[&b]);
        assert_eq!(labels[&a].len(), "npub…".len() + 3);
    }
}
