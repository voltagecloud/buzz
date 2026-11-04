use super::*;
use buzz_identity::{Identity, Purpose};
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
use nostr_wire::JsonUtil as _;

const CHANNEL: &str = "00000000-0000-4000-8000-000000000001";

fn signed(identity: &Identity, draft: Draft, at: u64) -> Event {
    identity
        .sign(
            Purpose::Content,
            identity
                .template(Purpose::Content, draft.kind, at, draft.tags, draft.content)
                .unwrap(),
        )
        .unwrap()
}

fn verified(event: Event) -> Verified {
    crate::Verifier::default().verify(event).unwrap()
}

#[test]
fn native_owner_grants_match_the_deployed_sdk_and_revoke_the_same_coordinate() {
    let identity = Identity::ephemeral();
    let agent = Identity::ephemeral().pubkey_hex();
    for enabled in [true, false] {
        let event = signed(
            &identity,
            Grant::draft(&agent, CHANNEL, enabled).unwrap(),
            100,
        );
        let native = Grant::parse(&verified(event.clone())).unwrap();
        let wire = nostr_wire::Event::from_json(serde_json::to_string(&event).unwrap()).unwrap();
        let server = buzz_sdk::sharing::Grant::parse(&wire).unwrap();
        assert_eq!(native.enabled, enabled);
        assert_eq!(server.enabled, enabled);
        assert_eq!(server.agent.to_hex(), agent);
        assert_eq!(server.channel.to_string(), CHANNEL);
        assert_eq!(event.pubkey, identity.public_key());
        assert!(event.verify().is_ok());
    }
}

#[test]
fn secure_signer_refuses_ambiguous_secret_bearing_or_self_grants() {
    let identity = Identity::ephemeral();
    let agent = Identity::ephemeral().pubkey_hex();
    let valid = || Grant::draft(&agent, CHANNEL, true).unwrap();
    let mut cases = Vec::new();
    for name in ["d", "h", "p"] {
        let mut draft = valid();
        draft.tags.push(vec![name.into(), "wrong".into()]);
        cases.push(draft);
    }
    let mut draft = valid();
    let mut value: serde_json::Value = serde_json::from_str(&draft.content).unwrap();
    value["private_key"] = "not-a-permission".into();
    draft.content = value.to_string();
    cases.push(draft);
    cases.push(Grant::draft(&identity.pubkey_hex(), CHANNEL, true).unwrap());
    for draft in cases {
        assert!(
            identity
                .template(
                    Purpose::Content,
                    draft.kind,
                    100,
                    draft.tags.clone(),
                    draft.content.clone()
                )
                .is_err()
        );
        // Callers can construct an unsigned event without using template().
        let unsigned = nostr::prelude::UnsignedEvent::new(
            identity.public_key(),
            Timestamp::from_secs(100),
            Kind::from_u16(draft.kind),
            draft.tags.into_iter().map(|tag| Tag::parse(tag).unwrap()),
            draft.content,
        );
        assert!(matches!(
            identity.sign(Purpose::Content, unsigned),
            Err(buzz_identity::IdentityError::NotAllowed)
        ));
    }
}

#[test]
fn parser_checks_scope_even_for_validly_signed_events() {
    let keys = Keys::generate();
    let agent = Identity::ephemeral().pubkey_hex();
    let draft = Grant::draft(&agent, CHANNEL, true).unwrap();
    let tags = draft
        .tags
        .into_iter()
        .map(|t| Tag::parse(t).unwrap())
        .chain([Tag::custom("h", ["wrong"])]);
    let event = EventBuilder::new(Kind::from_u16(kinds::AGENT_CHANNEL_GRANT), draft.content)
        .tags(tags)
        .finalize(&keys)
        .unwrap();
    assert!(Grant::parse(&verified(event)).is_err());
}

#[test]
fn current_authentication_conditions_determine_ownership() {
    let human = Identity::ephemeral();
    let keys = Keys::generate();
    let wire_human = nostr_wire::Keys::parse(human.nsec().as_str()).unwrap();
    let wire_agent = nostr_wire::PublicKey::from_hex(&keys.public_key().to_hex()).unwrap();
    for (conditions, allowed) in [
        ("", true),
        ("kind=22242&created_at<101", true),
        ("created_at<99", false),
        ("kind=9", false),
    ] {
        let auth =
            buzz_sdk::nip_oa::compute_auth_tag(&wire_human, &wire_agent, conditions).unwrap();
        let tag: Vec<String> = serde_json::from_str(&auth).unwrap();
        let event = EventBuilder::new(Kind::Metadata, "{}")
            .tags([Tag::parse(tag).unwrap()])
            .custom_created_at(Timestamp::from_secs(1))
            .finalize(&keys)
            .unwrap();
        assert_eq!(
            owner(&verified(event), 100).is_some(),
            allowed,
            "{conditions}"
        );
    }
}

#[test]
fn a_new_local_listing_disables_an_older_remote_listing() {
    let human = Identity::ephemeral();
    let agent = Identity::ephemeral().pubkey_hex();
    let listing = |runtime: &str, at| {
        verified(signed(
            &human,
            Draft {
                kind: kinds::MANAGED_AGENT,
                content: "{}".into(),
                tags: vec![
                    vec!["d".into(), agent.clone()],
                    vec!["runtime".into(), runtime.into()],
                ],
            },
            at,
        ))
    };
    let old = listing("voltage.remote-workspace/sandbox", 1);
    let new = listing("buzz.codex/codex", 2);
    assert!(remote_agents(&[new, old], &human.pubkey_hex()).is_empty());
    assert_eq!(
        remote_agents(
            &[listing("voltage.remote-workspace/sandbox", 3)],
            &human.pubkey_hex()
        ),
        [agent].into()
    );
}
