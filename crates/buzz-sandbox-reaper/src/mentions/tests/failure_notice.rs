use super::*;
use nostr::Keys;

fn fixture() -> (Identity, Event) {
    let owner = Keys::parse(&"33".repeat(32)).unwrap();
    let keys = Keys::parse(&"22".repeat(32)).unwrap();
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &keys.public_key(), "").unwrap();
    let identity = Identity {
        name: "fixture".into(),
        namespace: "fixture".into(),
        revision: "1".into(),
        registered_at: 0,
        owner: owner.public_key(),
        keys,
        auth: buzz_sdk::nip_oa::parse_auth_tag(&auth).unwrap(),
        relay: "ws://127.0.0.1:3000".into(),
    };
    let event = EventBuilder::new(Kind::Custom(9), "task text must not appear in the notice")
        .tags([
            Tag::parse(["h", "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10"]).unwrap(),
            Tag::public_key(identity.keys.public_key()),
        ])
        .sign_with_keys(&owner)
        .unwrap();
    (identity, event)
}

#[test]
fn denial_reports_the_reason_in_the_signed_thread_reply() {
    let (identity, event) = fixture();
    for reason in [
        "agent is private in this channel",
        "agent handoff requires task ancestry",
        "handoff parent does not match this task",
        "four agent handoffs already admitted; ask a human before continuing",
    ] {
        let notice = failure_notice(&identity, &event, "shared_task_denied", Some(reason)).unwrap();
        assert_eq!(notice.content, format!("Shared task refused: {reason}"));
        assert_eq!(notice.pubkey, identity.keys.public_key());
        assert!(notice.verify().is_ok());
        for tag in [
            vec!["h", "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10"],
            vec!["e", &event.id.to_hex(), "", "root"],
            vec!["e", &event.id.to_hex(), "", "reply"],
        ] {
            assert!(notice.tags.iter().any(|actual| actual.as_slice() == tag));
        }
    }
}

#[test]
fn missing_reasons_and_other_failures_keep_existing_notices() {
    let (identity, event) = fixture();
    for reason in [None, Some(""), Some("   ")] {
        let notice = failure_notice(&identity, &event, "shared_task_denied", reason).unwrap();
        assert!(notice
            .content
            .starts_with("Shared task refused: current sharing permission"));
    }
    for code in ["shared_task_retry", "launch_failed", "session_ended"] {
        let original = failure_notice(&identity, &event, code, None).unwrap();
        let with_detail =
            failure_notice(&identity, &event, code, Some("private upstream detail")).unwrap();
        assert_eq!(with_detail.content, original.content);
        assert!(!with_detail.content.contains("private upstream detail"));
    }
}
