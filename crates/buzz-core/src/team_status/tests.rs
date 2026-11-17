use super::*;
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

fn fixture(content: &str, change: impl FnOnce(&mut Vec<Vec<String>>)) -> (Event, CommunityId) {
    let community = CommunityId::from_uuid(Uuid::new_v4());
    let channel = Uuid::new_v4();
    let keys = Keys::generate();
    let agent = Keys::generate().public_key();
    let root = EventId::from_byte_array([7; 32]);
    let mut tags = vec![
        vec!["d".into(), address(community, agent, channel, root)],
        vec!["community".into(), community.to_string()],
        vec!["h".into(), channel.to_string()],
        vec!["p".into(), agent.to_hex()],
        vec!["e".into(), root.to_hex(), "".into(), "root".into()],
        vec!["expiration".into(), "1900".into()],
    ];
    change(&mut tags);
    let event = EventBuilder::new(Kind::Custom(KIND_TEAM_AGENT_STATUS as u16), content)
        .custom_created_at(Timestamp::from(1000))
        .tags(tags.into_iter().map(|t| Tag::parse(t).unwrap()))
        .sign_with_keys(&keys)
        .unwrap();
    (event, community)
}

#[test]
fn coarse_labels_and_clear_preserve_scope() {
    for status in [
        Status::Working,
        Status::Waiting,
        Status::Blocked,
        Status::Done,
    ] {
        for caption in [None, Some("DEV-2868 currency plumbing")] {
            let label = caption.map_or_else(
                || status.as_str().to_owned(),
                |c| format!("{}: {c}", status.as_str()),
            );
            let content = serde_json::json!({"v":1,"label":label}).to_string();
            let (event, community) = fixture(&content, |_| {});
            let parsed = parse(&event, community).unwrap();
            assert_eq!(
                parsed.content,
                Content::Label {
                    status,
                    caption: caption.map(str::to_owned)
                }
            );
            assert_eq!(
                parsed.address,
                address(community, parsed.agent, parsed.channel, parsed.root)
            );
            assert!(validate_time(&event, &parsed, 1000).is_ok());
        }
    }
    let (event, community) = fixture("", |_| {});
    assert_eq!(parse(&event, community).unwrap().content, Content::Clear);
}

#[test]
fn malformed_payloads_and_telemetry_are_rejected() {
    for content in [
        " ",
        "null",
        "[]",
        r#"{"v":2,"label":"working"}"#,
        r#"{"v":1,"label":"working","tool":"shell"}"#,
        r#"{"v":1,"v":1,"label":"working"}"#,
        r#"{"v":1,"label":"working","label":"done"}"#,
    ] {
        assert!(parse_content(content).is_err(), "{content}");
    }
    for label in [
        "online",
        "working ",
        "working:",
        "working: ",
        "working:  caption",
        "working: @alice",
        "working: nostr:npub123",
        "working: /tmp/task",
        "working: https://example.org",
        "working: `cargo test`",
        "working\ndone",
        "working: line\u{2028}break",
    ] {
        assert!(
            parse_content(&serde_json::json!({"v":1,"label":label}).to_string()).is_err(),
            "{label}"
        );
    }
    let label = format!("working: {}", "é".repeat(MAX_LABEL_CHARS - 9));
    assert!(parse_content(&serde_json::json!({"v":1,"label":label}).to_string()).is_ok());
    assert!(
        parse_content(&serde_json::json!({"v":1,"label":format!("{label}é")}).to_string()).is_err()
    );
    let encoded = format!(
        "{{\"v\":1,\"label\":\"working\"}}{}",
        " ".repeat(MAX_CONTENT_BYTES)
    );
    assert!(parse_content(&encoded).is_err());
}

#[test]
fn routing_cannot_be_ambiguous_or_replayed_across_communities() {
    for content in ["", r#"{"v":1,"label":"working"}"#] {
        let (event, community) = fixture(content, |_| {});
        assert!(parse(&event, CommunityId::from_uuid(Uuid::new_v4())).is_err());
        assert!(parse(&event, community).is_ok());
        for index in 0..6 {
            let (event, community) = fixture(content, |tags| {
                tags.remove(index);
            });
            assert!(parse(&event, community).is_err(), "missing {index}");
            let (event, community) = fixture(content, |tags| {
                tags.push(tags[index].clone());
            });
            assert!(parse(&event, community).is_err(), "duplicate {index}");
            let (event, community) = fixture(content, |tags| {
                tags[index][1] = "invalid".into();
            });
            assert!(parse(&event, community).is_err(), "invalid {index}");
        }
        let (event, community) = fixture(content, |tags| {
            tags[4][3] = "reply".into();
        });
        assert!(parse(&event, community).is_err());
        let (event, community) = fixture(content, |tags| {
            tags[4][2] = "wss://relay.example".into();
        });
        assert!(parse(&event, community).is_err());
        let (event, community) = fixture(content, |tags| {
            tags[0][1].push('x');
        });
        assert!(parse(&event, community).is_err());
    }
}

#[test]
fn expired_heads_parse_but_cannot_be_republished() {
    let (event, community) = fixture("", |_| {});
    let parsed = parse(&event, community).unwrap();
    assert!(validate_time(&event, &parsed, 940).is_ok());
    assert!(validate_time(&event, &parsed, 939).is_err());
    assert!(validate_time(&event, &parsed, 1899).is_ok());
    assert!(validate_time(&event, &parsed, 1900).is_err());
    assert!(parse(&event, community).is_ok());
    for expiry in ["1000", "999", "1901", "01900", "18446744073709551615"] {
        let (event, community) = fixture("", |tags| {
            tags[5][1] = expiry.into();
        });
        assert!(parse(&event, community).is_err(), "{expiry}");
    }
}

#[test]
fn status_delivery_requires_an_explicit_kind_or_id() {
    let (event, _) = fixture(r#"{"v":1,"label":"working"}"#, |_| {});
    let stored =
        crate::StoredEvent::with_received_at(event.clone(), chrono::Utc::now(), None, true);
    assert!(!crate::filter::filters_match(
        &[nostr::Filter::new()],
        &stored
    ));
    assert!(crate::filter::filters_match(
        &[nostr::Filter::new().id(event.id)],
        &stored
    ));
    assert!(crate::filter::filters_match(
        &[nostr::Filter::new().kind(event.kind)],
        &stored
    ));
}
