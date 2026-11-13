use super::*;
use serde_json::json;

const CHANNEL: &str = "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10";

fn root() -> EventId {
    EventId::from_hex(&"ab".repeat(32)).unwrap()
}

fn payload() -> TranscriptPayload {
    TranscriptPayload {
        v: PAYLOAD_VERSION,
        turn_id: "turn-1".into(),
        turn_seq: 7,
        thread_root: root().to_hex(),
        started_at: "2026-11-13T10:00:00Z".into(),
        ended_at: "2026-11-13T10:01:00.250Z".into(),
        stop_reason: "end_turn".into(),
        usage: Some(TranscriptUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..TranscriptUsage::default()
        }),
        context: Some(ContextWindow {
            size: 200_000,
            used: 51_234,
        }),
        workspace: vec![WorkspaceEntry {
            path: "/home/agent/backend".into(),
            repo: "voltagecloud/backend".into(),
            branch: Some("feat/x".into()),
            head: "c".repeat(40),
            pushed: true,
        }],
        omitted: 0,
        updates: vec![
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}),
            json!({"sessionUpdate":"tool_call","toolCallId":"t1","title":"Read","kind":"read",
                   "status":"completed","locations":[{"path":"/home/agent/backend/a.rs"}]}),
        ],
    }
}

fn envelope(recipient: PublicKey) -> TranscriptEnvelope {
    TranscriptEnvelope {
        recipient,
        thread_root: root(),
        channel: CHANNEL.parse().unwrap(),
        turn_id: "turn-1".into(),
        part_index: 0,
        part_count: 1,
    }
}

fn sealed(agent: &Keys, recipient: PublicKey, payload: &TranscriptPayload) -> Event {
    let content = encrypt_part(agent, &recipient, payload).unwrap();
    build_agent_session_transcript(&envelope(recipient), &content)
        .unwrap()
        .sign_with_keys(agent)
        .unwrap()
}

#[test]
fn part_round_trips_for_its_recipient_only() {
    let (agent, owner, other) = (Keys::generate(), Keys::generate(), Keys::generate());
    let event = sealed(&agent, owner.public_key(), &payload());
    assert_eq!(
        event
            .tags
            .iter()
            .map(|t| t.as_slice().to_vec())
            .collect::<Vec<_>>(),
        vec![
            vec!["p".to_string(), owner.public_key().to_hex()],
            vec!["e".into(), root().to_hex(), "".into(), "root".into()],
            vec!["h".into(), CHANNEL.into()],
            vec!["turn".into(), "turn-1".into(), "0".into(), "1".into()],
        ],
        "only the four envelope tags appear in cleartext"
    );
    let (opened_envelope, opened) =
        open_agent_session_transcript(&owner, &agent.public_key(), &event).unwrap();
    assert_eq!(opened, payload());
    assert_eq!(opened_envelope, envelope(owner.public_key()));

    assert!(matches!(
        open_agent_session_transcript(&other, &agent.public_key(), &event),
        Err(TranscriptError::NotRecipient)
    ));
    assert!(matches!(
        open_agent_session_transcript(&owner, &other.public_key(), &event),
        Err(TranscriptError::UnexpectedAuthor)
    ));
}

#[test]
fn open_rejects_payloads_that_disagree_with_the_tags_or_the_bounds() {
    let (agent, owner) = (Keys::generate(), Keys::generate());
    let raw = |plaintext: &str| {
        let content = nip44::encrypt(
            agent.secret_key(),
            &owner.public_key(),
            plaintext,
            nip44::Version::V2,
        )
        .unwrap();
        build_agent_session_transcript(&envelope(owner.public_key()), &content)
            .unwrap()
            .sign_with_keys(&agent)
            .unwrap()
    };
    let open = |event: &Event| open_agent_session_transcript(&owner, &agent.public_key(), event);

    let mut other_turn = payload();
    other_turn.turn_id = "turn-2".into();
    assert!(matches!(
        open(&raw(&serde_json::to_string(&other_turn).unwrap())),
        Err(TranscriptError::Payload(_))
    ));

    let mut other_root = payload();
    other_root.thread_root = "cd".repeat(32);
    assert!(matches!(
        open(&raw(&serde_json::to_string(&other_root).unwrap())),
        Err(TranscriptError::Payload(_))
    ));

    // Over 60 KiB of plaintext, though still within NIP-44's own limit.
    let mut huge = payload();
    huge.updates = vec![json!({"sessionUpdate":"plan","entries":["x".repeat(62_000)]})];
    let huge = serde_json::to_string(&huge).unwrap();
    assert!(huge.len() > MAX_PART_PLAINTEXT_BYTES && huge.len() < 65_535);
    assert!(matches!(
        open(&raw(&huge)),
        Err(TranscriptError::PlaintextTooLarge { .. })
    ));

    let mut rejected = payload();
    rejected.updates = vec![json!({"sessionUpdate":"plan","entries":["x".repeat(62_000)]})];
    assert!(matches!(
        encrypt_part(&agent, &owner.public_key(), &rejected),
        Err(TranscriptError::PlaintextTooLarge { .. })
    ));
}

#[test]
fn payload_rules_reject_each_malformed_field() {
    type Mutate = fn(&mut TranscriptPayload);
    let cases: [(&str, Mutate); 10] = [
        ("version", |p| p.v = 2),
        ("turn id", |p| p.turn_id = "has space".into()),
        ("thread root", |p| p.thread_root = "AB".repeat(32)),
        ("started_at", |p| p.started_at = "yesterday".into()),
        ("stop reason", |p| p.stop_reason = "End Turn".into()),
        ("usage_update is not kept", |p| {
            p.updates
                .push(json!({"sessionUpdate":"usage_update","size":1,"used":1}))
        }),
        ("tool call without id", |p| {
            p.updates
                .push(json!({"sessionUpdate":"tool_call","title":"x"}))
        }),
        ("non-GitHub repo name", |p| {
            p.workspace[0].repo = "gitlab.com/a/b".into()
        }),
        ("relative path", |p| p.workspace[0].path = "backend".into()),
        ("too many workspace entries", |p| {
            p.workspace = vec![p.workspace[0].clone(); MAX_WORKSPACE_ENTRIES + 1]
        }),
    ];
    assert!(payload().validate().is_ok());
    for (name, mutate) in cases {
        let mut p = payload();
        mutate(&mut p);
        assert!(p.validate().is_err(), "{name} must be rejected");
    }
}

#[test]
fn envelope_parser_accepts_only_the_canonical_tag_set() {
    let agent = Keys::generate();
    let owner: &'static str = Keys::generate().public_key().to_hex().leak();
    let ciphertext = format!("Ag{}", "A".repeat(130));
    let root: &'static str = root().to_hex().leak();
    let event = |tags: Vec<Vec<&str>>, content: &str| {
        EventBuilder::new(
            Kind::Custom(KIND_AGENT_SESSION_TRANSCRIPT as u16),
            content.to_owned(),
        )
        .tags(tags.into_iter().map(|t| Tag::parse(t).unwrap()))
        .sign_with_keys(&agent)
        .unwrap()
    };
    let canonical = || {
        vec![
            vec!["p", owner],
            vec!["e", root, "", "root"],
            vec!["h", CHANNEL],
            vec!["turn", "turn-1", "0", "2"],
        ]
    };
    let parsed = parse_envelope(&event(canonical(), &ciphertext)).unwrap();
    assert_eq!((parsed.part_index, parsed.part_count), (0, 2));

    type Mutate = fn(&mut Vec<Vec<&'static str>>);
    let cases: [(&str, Mutate); 13] = [
        ("missing p", |t| {
            t.remove(0);
        }),
        ("second p", |t| {
            t.push(vec!["p", "cc".repeat(32).leak()]);
        }),
        ("missing e", |t| {
            t.remove(1);
        }),
        ("reply marker", |t| t[1][3] = "reply"),
        ("relay hint", |t| t[1][2] = "wss://relay"),
        ("missing h", |t| {
            t.remove(2);
        }),
        ("uppercase h", |t| {
            t[2][1] = "0B6C1F0E-2A7D-4C1E-9F3A-5D2B8E4C7A10";
        }),
        ("missing turn", |t| {
            t.remove(3);
        }),
        ("index past count", |t| t[3][2] = "2"),
        ("too many parts", |t| t[3][3] = "9"),
        ("leading zero", |t| t[3][2] = "00"),
        ("extra agent tag", |t| t.push(vec!["agent", "x"])),
        ("extra expiration tag", |t| t.push(vec!["expiration", "1"])),
    ];
    for (name, mutate) in cases {
        let mut tags = canonical();
        mutate(&mut tags);
        assert!(
            parse_envelope(&event(tags, &ciphertext)).is_err(),
            "{name} must be rejected"
        );
    }
    assert!(
        parse_envelope(&event(canonical(), "plaintext transcript")).is_err(),
        "plaintext content must be rejected"
    );
}
