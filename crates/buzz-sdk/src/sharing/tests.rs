use super::*;
use nostr::{Keys, Timestamp};

#[test]
fn signed_grants_round_trip_and_revocations_keep_the_coordinate() {
    let owner = Keys::generate();
    let agent = Keys::generate().public_key();
    let channel = Uuid::new_v4();
    let make = |enabled| {
        Grant::builder(agent, channel, enabled)
            .unwrap()
            .sign_with_keys(&owner)
            .unwrap()
    };
    for enabled in [true, false] {
        let event = make(enabled);
        let grant = Grant::parse(&event).unwrap();
        assert_eq!(
            grant,
            Grant {
                version: 1,
                agent,
                channel,
                enabled
            }
        );
        assert!(event
            .tags
            .iter()
            .any(|t| t.as_slice() == ["d", &Grant::coordinate(agent, channel)]));
    }
    assert_ne!(
        Grant::coordinate(agent, channel),
        Grant::coordinate(agent, Uuid::new_v4())
    );
}

#[test]
fn altered_signed_content_and_ambiguous_tags_are_rejected() {
    let owner = Keys::generate();
    let agent = Keys::generate().public_key();
    let channel = Uuid::new_v4();
    let builder = || Grant::builder(agent, channel, true).unwrap();
    let mut event = builder().sign_with_keys(&owner).unwrap();
    event.content = event.content.replace("true", "false");
    assert!(Grant::parse(&event).is_err());
    for tag in [vec!["h", "wrong"], vec!["p", "wrong"], vec!["d", "wrong"]] {
        let event = builder()
            .tag(Tag::parse(tag).unwrap())
            .sign_with_keys(&owner)
            .unwrap();
        assert!(Grant::parse(&event).is_err());
    }
    let own = Grant::builder(owner.public_key(), channel, true)
        .unwrap()
        .sign_with_keys(&owner)
        .unwrap();
    assert!(Grant::parse(&own).is_err());
    for body in ["{}", "{\"version\":2}", "null"] {
        let event = EventBuilder::new(Kind::Custom(KIND_AGENT_CHANNEL_GRANT as u16), body)
            .sign_with_keys(&owner)
            .unwrap();
        assert!(Grant::parse(&event).is_err());
    }
}

#[test]
fn only_a_valid_current_owner_attestation_proves_ownership() {
    let owner = Keys::generate();
    let agent = Keys::generate();
    let now = Timestamp::now().as_secs();
    let auth = nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
    let tag = nip_oa::parse_auth_tag(&auth).unwrap();
    let event = EventBuilder::new(Kind::Metadata, "{}")
        .tag(tag.clone())
        .sign_with_keys(&agent)
        .unwrap();
    assert_eq!(profile_owner(&event, now).unwrap(), owner.public_key());
    let other = Keys::generate();
    let forged = EventBuilder::new(Kind::Metadata, "{}")
        .tag(tag.clone())
        .sign_with_keys(&other)
        .unwrap();
    assert!(profile_owner(&forged, now).is_err());
    let expired =
        nip_oa::compute_auth_tag(&owner, &agent.public_key(), &format!("created_at<{now}"))
            .unwrap();
    let expired = EventBuilder::new(Kind::Metadata, "{}")
        .tag(nip_oa::parse_auth_tag(&expired).unwrap())
        .sign_with_keys(&agent)
        .unwrap();
    assert!(profile_owner(&expired, now).is_err());
    let human = EventBuilder::new(Kind::Metadata, "{}")
        .sign_with_keys(&owner)
        .unwrap();
    assert!(profile_owner(&human, now).is_err());
}
