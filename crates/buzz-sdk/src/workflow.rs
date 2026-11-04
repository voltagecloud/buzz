//! Verified owner delegation carried by relay-authored workflow prompts.

use std::collections::HashSet;

use nostr::{Event, PublicKey, Tag};

/// Identify the workflow owner only when the active relay signed a kind:9
/// prompt explicitly targeting this agent in the owner's stored template.
/// Routing `p` tags alone never convey delegated authority. Callers obtain
/// `relay` from the authenticated community's NIP-11 `self` key.
pub fn owner(event: &Event, relay: PublicKey, agent: PublicKey) -> Option<PublicKey> {
    if event.kind.as_u16() as u32 != crate::kind::KIND_STREAM_MESSAGE
        || event.pubkey != relay
        || event.verify().is_err()
    {
        return None;
    }
    let tags = |name: &str| {
        event
            .tags
            .iter()
            .map(Tag::as_slice)
            .filter(|tag| tag.first().map(String::as_str) == Some(name))
            .collect::<Vec<_>>()
    };
    if tags("buzz:workflow").as_slice() != [["buzz:workflow", "true"]] {
        return None;
    }
    let owners = tags("buzz:workflow-owner");
    let [owner] = owners.as_slice() else {
        return None;
    };
    let [_, owner] = *owner else {
        return None;
    };
    let parsed = PublicKey::from_hex(owner).ok()?;
    if parsed.to_hex() != *owner {
        return None;
    }
    let mut mentioned = HashSet::new();
    for tag in tags("buzz:workflow-mention") {
        let [_, value] = tag else {
            return None;
        };
        let key = PublicKey::from_hex(value).ok()?;
        if key.to_hex() != *value || !mentioned.insert(key) {
            return None;
        }
    }
    mentioned.contains(&agent).then_some(parsed)
}
