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

/// Relay assertion for a stored schedule trigger with a literal prompt template.
pub const SCHEDULED_LITERAL_TAG: &str = "buzz:workflow-schedule";

/// Verify the relay's scheduled-literal assertion before attributing a gateway prompt.
/// Delegation and current channel membership remain separate caller checks.
pub fn gateway_owner(event: &Event, relay: PublicKey, agent: PublicKey) -> Option<PublicKey> {
    let proof = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some(SCHEDULED_LITERAL_TAG))
        .map(Tag::as_slice)
        .collect::<Vec<_>>();
    if proof.as_slice() != [[SCHEDULED_LITERAL_TAG, "literal-v1"]] {
        return None;
    }
    owner(event, relay, agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind};

    #[test]
    fn gateway_workflow_authority_requires_one_relay_signed_schedule_assertion() {
        let relay = Keys::generate();
        let gateway = Keys::generate().public_key();
        let agent = Keys::generate().public_key();
        let forged = Keys::generate();
        for (proofs, signer, expected) in [
            (vec!["literal-v1"], &relay, true),
            (vec![], &relay, false),
            (vec!["literal-v2"], &relay, false),
            (vec!["literal-v1", "literal-v1"], &relay, false),
            (vec!["literal-v1"], &forged, false),
        ] {
            let mut tags = vec![
                Tag::parse(["buzz:workflow", "true"]).unwrap(),
                Tag::parse(["buzz:workflow-owner", &gateway.to_hex()]).unwrap(),
                Tag::parse(["buzz:workflow-mention", &agent.to_hex()]).unwrap(),
            ];
            tags.extend(
                proofs
                    .into_iter()
                    .map(|proof| Tag::parse([SCHEDULED_LITERAL_TAG, proof]).unwrap()),
            );
            let event = EventBuilder::new(Kind::Custom(9), "fixed instruction")
                .tags(tags)
                .sign_with_keys(signer)
                .unwrap();
            assert_eq!(
                gateway_owner(&event, relay.public_key(), agent),
                expected.then_some(gateway)
            );
        }
    }
}
