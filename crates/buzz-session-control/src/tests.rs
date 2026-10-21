use super::*;
use buzz_core::kind::{EPHEMERAL_KIND_MAX, EPHEMERAL_KIND_MIN};
use nostr::nips::nip19::ToBech32;
use serde_json::{json, Value};

/// Fixture secrets. None may appear anywhere in a serialized event.
const API_KEY: &str = "sk-ant-fixture-api-key-do-not-leak-0001";
const OAUTH_TOKEN: &str = "oauth-fixture-token-do-not-leak-0002";

/// Payload field names and values that must stay inside the ciphertext.
/// Six or more characters, so a chance match in base64 is negligible.
const PAYLOAD_WORDS: &[&str] = &[
    "request_id",
    "payload",
    "deploy",
    "private_key_nsec",
    "auth_tag",
    "relay_url",
    "launch",
    "owner_pubkey",
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "provider_config",
    "sandbox",
    "channel_id",
    "thread_root",
    "status",
    "accepted",
    "response",
    "agent_id",
    "session_ended",
];

fn keys(byte: &str) -> Keys {
    Keys::parse(&byte.repeat(32)).unwrap()
}

fn manager() -> Keys {
    keys("11")
}

fn agent() -> Keys {
    keys("22")
}

fn owner() -> Keys {
    keys("33")
}

fn now() -> Timestamp {
    Timestamp::from(1_800_000_000)
}

fn auth_tag() -> String {
    buzz_sdk::nip_oa::compute_auth_tag(&owner(), &agent().public_key(), "").unwrap()
}

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A deploy request carrying an nsec, an API key and an OAuth token.
fn secret_request() -> Request {
    serde_json::from_str(&fixture("deploy.request.envelope.json")).unwrap()
}

fn request_event(request: &Request) -> Event {
    build_request(
        &agent(),
        &manager().public_key(),
        Some(&auth_tag()),
        request,
        now(),
    )
    .unwrap()
}

fn reply_event(request: &EventId, reply: &Reply) -> Event {
    build_reply(&manager(), &agent().public_key(), request, reply, now()).unwrap()
}

fn refusal() -> Reply {
    serde_json::from_str(&fixture("refusal.reply.envelope.json")).unwrap()
}

/// Sign an event with arbitrary content and tags, as a hostile or broken
/// sender would, bypassing the builders.
fn forged(signer: &Keys, content: &str, tags: Vec<Tag>) -> Event {
    EventBuilder::new(Kind::Custom(KIND), content)
        .tags(tags)
        .custom_created_at(now())
        .sign_with_keys(signer)
        .unwrap()
}

fn tag_names(event: &Event) -> Vec<String> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice()[0].clone())
        .collect()
}

/// Assert that none of the secrets or payload words appear anywhere in the
/// full serialized event, and that nothing but the NIP-01 fields is present.
fn assert_opaque(event: &Event, secrets: &[&str]) {
    let json = serde_json::to_string(event).unwrap();
    for needle in secrets.iter().chain(PAYLOAD_WORDS) {
        assert!(!json.contains(needle), "{needle:?} is visible in {json}");
    }
    let value: Value = serde_json::from_str(&json).unwrap();
    let mut fields: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "content",
            "created_at",
            "id",
            "kind",
            "pubkey",
            "sig",
            "tags"
        ]
    );
    assert!(buzz_core::observer::content_looks_like_nip44(
        &event.content
    ));
}

#[test]
fn kind_is_24210_and_ephemeral() {
    assert_eq!(KIND_SANDBOX_SESSION_CONTROL, 24210);
    assert!((EPHEMERAL_KIND_MIN..=EPHEMERAL_KIND_MAX).contains(&KIND_SANDBOX_SESSION_CONTROL));
    assert_eq!(request_event(&secret_request()).kind, Kind::Custom(24210));
}

/// Historical bytes: events recorded by this crate's first version must keep
/// opening to the same envelopes.
#[test]
fn golden_events_open_to_their_envelopes() {
    let event: Event = serde_json::from_str(&fixture("deploy.request.event.json")).unwrap();
    let opened = open_request(&event, &manager()).unwrap();
    assert_eq!(opened.request, secret_request());
    assert_eq!(opened.signer, agent().public_key());
    assert_eq!(opened.created_at, now());
    assert_eq!(opened.expiration, Some(Timestamp::from(1_800_000_120)));
    let tag = opened.auth_tag.unwrap();
    assert_eq!(
        buzz_sdk::nip_oa::verify_auth_tag(&tag, &opened.signer).unwrap(),
        owner().public_key()
    );
    for name in ["accepted", "done", "refusal"] {
        let reply: Event =
            serde_json::from_str(&fixture(&format!("{name}.reply.event.json"))).unwrap();
        let expected: Reply =
            serde_json::from_str(&fixture(&format!("{name}.reply.envelope.json"))).unwrap();
        let opened = open_reply(&reply, &agent(), &manager().public_key(), &event.id).unwrap();
        assert_eq!(opened, expected, "{name}");
    }
}

#[test]
fn envelopes_round_trip_for_every_op_and_status() {
    for op in [Op::Info, Op::Deploy, Op::Stop, Op::Enroll] {
        let request = Request::new(op, "r-1", json!({"op": op.as_str()}));
        let auth = (op != Op::Enroll).then(auth_tag);
        let event = build_request(
            &agent(),
            &manager().public_key(),
            auth.as_deref(),
            &request,
            now(),
        )
        .unwrap();
        let opened = open_request(&event, &manager()).unwrap();
        assert_eq!(opened.request, request);
        assert_eq!(opened.event_id, event.id);
        assert_eq!(opened.auth_tag.is_some(), op != Op::Enroll);
        assert_eq!(opened.expiration, Some(now() + EXPIRATION_SECS));
    }
    let request = request_event(&secret_request());
    for reply in [
        Reply::accepted("r-1"),
        Reply::progress("r-1"),
        Reply::done("r-1", json!({"ok": true, "agent_id": "a"})),
    ] {
        let event = reply_event(&request.id, &reply);
        let opened = open_reply(&event, &agent(), &manager().public_key(), &request.id).unwrap();
        assert_eq!(opened, reply);
    }
}

/// Confidentiality rules 1 and 2: the full serialized request and reply,
/// built with real secrets, show none of them and no payload field name.
#[test]
fn serialized_events_reveal_no_secret_and_no_payload_field() {
    let request = secret_request();
    let nsec = agent().secret_key().to_bech32().unwrap();
    let secrets = [nsec.as_str(), API_KEY, OAUTH_TOKEN, &request.request_id];
    let event = request_event(&request);
    assert_opaque(&event, &secrets);
    assert_eq!(tag_names(&event), ["p", "auth", "expiration"]);

    let done = Reply::done(
        request.request_id.clone(),
        json!({"ok": true, "agent_id": "buzz-agent-466d7fcae563", "echo": [nsec, API_KEY, OAUTH_TOKEN]}),
    );
    let reply = reply_event(&event.id, &done);
    assert_opaque(&reply, &secrets);
    assert_eq!(tag_names(&reply), ["p", "e", "expiration"]);
}

/// Confidentiality rule 4c: a refusal is as opaque as any other reply.
#[test]
fn refusal_replies_are_encrypted() {
    let reply = refusal();
    let request = request_event(&secret_request());
    let event = reply_event(&request.id, &reply);
    assert_opaque(&event, &["session ended", "stopped"]);
    let opened = open_reply(&event, &agent(), &manager().public_key(), &request.id).unwrap();
    assert_eq!(opened.response.unwrap()["code"], "session_ended");
}

/// Confidentiality rule 4b: plain JSON, NIP-04 and unknown NIP-44 versions
/// are refused before any parsing, at every length.
#[test]
fn opener_refuses_content_that_is_not_nip44_v2() {
    let p = || vec![Tag::public_key(manager().public_key())];
    let envelope = serde_json::to_string(&secret_request()).unwrap();
    assert!(envelope.len() > buzz_core::observer::NIP44_MIN_CONTENT_LEN);
    let nip04 = format!("{}?iv=AAAAAAAAAAAAAAAAAAAAAA==", "A".repeat(200));
    let v1 = format!("AQ{}", "A".repeat(200));
    for content in [
        r#"{"v":1,"op":"info","request_id":"r","payload":{}}"#,
        envelope.as_str(),
        nip04.as_str(),
        v1.as_str(),
        "",
    ] {
        let event = forged(&agent(), content, p());
        assert_eq!(
            open_request(&event, &manager()).unwrap_err(),
            WireError::NotCiphertext,
            "{content:.40}"
        );
    }
    let request = request_event(&secret_request()).id;
    let reply = forged(
        &manager(),
        &serde_json::to_string(&refusal()).unwrap(),
        vec![Tag::public_key(agent().public_key()), Tag::event(request)],
    );
    assert_eq!(
        open_reply(&reply, &agent(), &manager().public_key(), &request).unwrap_err(),
        WireError::NotCiphertext
    );
}

/// Confidentiality rule 4b: ciphertext addressed to the manager in the `p`
/// tag but encrypted to another key does not open.
#[test]
fn opener_refuses_content_encrypted_to_another_key() {
    let other = keys("44");
    let plaintext = serde_json::to_string(&secret_request()).unwrap();
    let ciphertext = nip44::encrypt(
        agent().secret_key(),
        &other.public_key(),
        &plaintext,
        nip44::Version::V2,
    )
    .unwrap();
    let event = forged(
        &agent(),
        &ciphertext,
        vec![Tag::public_key(manager().public_key())],
    );
    assert_eq!(
        open_request(&event, &manager()).unwrap_err(),
        WireError::Undecryptable
    );
}

#[test]
fn opener_refuses_events_for_someone_else() {
    let event = request_event(&secret_request());
    assert_eq!(
        open_request(&event, &keys("44")).unwrap_err(),
        WireError::WrongRecipient
    );
    let reply = reply_event(&event.id, &Reply::accepted("r"));
    let manager_pk = manager().public_key();
    assert_eq!(
        open_reply(&reply, &keys("44"), &manager_pk, &event.id).unwrap_err(),
        WireError::WrongRecipient
    );
    assert_eq!(
        open_reply(&reply, &agent(), &keys("44").public_key(), &event.id).unwrap_err(),
        WireError::WrongSender
    );
    let other_request = request_event(&Request::new(Op::Info, "r-2", json!({})));
    assert_eq!(
        open_reply(&reply, &agent(), &manager_pk, &other_request.id).unwrap_err(),
        WireError::WrongRequest
    );
}

#[test]
fn opener_refuses_tampered_events() {
    let event = request_event(&secret_request());
    let other = request_event(&Request::new(Op::Info, "r-2", json!({})));
    let mut json: Value = serde_json::to_value(&event).unwrap();
    let tampers: [(&str, Value); 4] = [
        ("content", json!(other.content)),
        ("created_at", json!(event.created_at.as_secs() + 1)),
        ("tags", json!([["p", manager().public_key().to_hex()]])),
        ("sig", json!(other.sig.to_string())),
    ];
    for (field, value) in tampers {
        let original = json[field].clone();
        json[field] = value;
        let tampered: Event = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            open_request(&tampered, &manager()).unwrap_err(),
            WireError::BadSignature,
            "{field}"
        );
        json[field] = original;
    }
    let wrong_kind = EventBuilder::new(Kind::Custom(1059), event.content.clone())
        .tags(event.tags.clone())
        .sign_with_keys(&agent())
        .unwrap();
    assert_eq!(
        open_request(&wrong_kind, &manager()).unwrap_err(),
        WireError::WrongKind
    );
}

/// Confidentiality rule 2: anything outside `p`, `e`, `auth` and
/// `expiration`, or a repeated tag, is refused even when the content is fine.
#[test]
fn opener_refuses_tags_outside_the_allowed_set() {
    let event = request_event(&secret_request());
    let manager_p = Tag::public_key(manager().public_key());
    let extra = [
        Tag::parse(["op", "deploy"]).unwrap(),
        Tag::parse(["h", "channel"]).unwrap(),
        manager_p.clone(),
        Tag::event(event.id),
        Tag::parse(["p", "not-hex"]).unwrap(),
        Tag::parse(["expiration", "soon"]).unwrap(),
    ];
    for tag in extra {
        let mut tags: Vec<Tag> = event.tags.iter().cloned().collect();
        tags.push(tag.clone());
        let forged = forged(&agent(), &event.content, tags);
        assert_eq!(
            open_request(&forged, &manager()).unwrap_err(),
            WireError::UnexpectedTag,
            "{:?}",
            tag.as_slice()
        );
    }
    let reply = reply_event(&event.id, &Reply::accepted("r"));
    let mut tags: Vec<Tag> = reply.tags.iter().cloned().collect();
    tags.push(buzz_sdk::nip_oa::parse_auth_tag(&auth_tag()).unwrap());
    let forged = forged(&manager(), &reply.content, tags);
    assert_eq!(
        open_reply(&forged, &agent(), &manager().public_key(), &event.id).unwrap_err(),
        WireError::UnexpectedTag
    );
}

#[test]
fn malformed_auth_tags_are_refused_when_building_and_opening() {
    let request = Request::new(Op::Info, "r", json!({}));
    assert_eq!(
        build_request(
            &agent(),
            &manager().public_key(),
            Some("synthetic-secret"),
            &request,
            now()
        )
        .unwrap_err(),
        WireError::MalformedAuthTag
    );
    let event = request_event(&request);
    let mut tags: Vec<Tag> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice()[0] != "auth")
        .cloned()
        .collect();
    tags.push(Tag::parse(["auth", "zz", "", "zz"]).unwrap());
    assert_eq!(
        open_request(&forged(&agent(), &event.content, tags), &manager()).unwrap_err(),
        WireError::MalformedAuthTag
    );
}

/// An unknown version or op still yields the request id, so the manager can
/// answer with a typed refusal instead of leaving the client to time out.
#[test]
fn unknown_version_and_op_are_refused_with_their_request_id() {
    let mut request = Request::new(Op::Info, "r-v2", json!({}));
    request.v = 2;
    assert_eq!(
        open_request(&request_event(&request), &manager()).unwrap_err(),
        WireError::UnsupportedVersion {
            version: 2,
            request_id: "r-v2".into()
        }
    );
    let sealed = |envelope: Value| {
        seal(
            &agent(),
            &manager().public_key(),
            &envelope,
            vec![Tag::public_key(manager().public_key())],
            now(),
        )
        .unwrap()
    };
    let unknown_op = sealed(json!({"v": 1, "op": "undeploy", "request_id": "r-op", "payload": {}}));
    assert_eq!(
        open_request(&unknown_op, &manager()).unwrap_err(),
        WireError::UnsupportedOp {
            request_id: "r-op".into()
        }
    );
    for malformed in [
        json!({"op": "info", "request_id": "r", "payload": {}}),
        json!({"v": 1, "op": "info", "payload": {}}),
        json!({"v": 1, "op": "info", "request_id": "", "payload": {}}),
        json!({"v": 1, "op": "info", "request_id": "has space", "payload": {}}),
        json!({"v": 1, "op": "info", "request_id": "r".repeat(MAX_REQUEST_ID_BYTES + 1), "payload": {}}),
        json!({"v": 1, "request_id": "r", "payload": {}}),
        json!({"v": 1, "op": "info", "request_id": "r"}),
        json!(["not", "an", "object"]),
    ] {
        assert_eq!(
            open_request(&sealed(malformed.clone()), &manager()).unwrap_err(),
            WireError::MalformedEnvelope,
            "{malformed}"
        );
    }
}

#[test]
fn freshness_allows_sixty_seconds_of_skew_and_honours_expiration() {
    let opened = open_request(&request_event(&secret_request()), &manager()).unwrap();
    let at = |offset: i64| Timestamp::from(now().as_secs().checked_add_signed(offset).unwrap());
    for (offset, fresh) in [
        (0, true),
        (60, true),
        (-60, true),
        (61, false),
        (-61, false),
    ] {
        assert_eq!(
            opened.check_fresh(at(offset)).is_ok(),
            fresh,
            "now {offset:+}s"
        );
        assert_eq!(
            check_created_at(now(), at(offset)).is_ok(),
            fresh,
            "{offset:+}s"
        );
    }
    let mut expired = opened.clone();
    expired.expiration = Some(at(-1));
    assert_eq!(expired.check_fresh(now()), Err(WireError::Stale));
    expired.expiration = Some(now());
    assert_eq!(expired.check_fresh(now()), Err(WireError::Stale));
}

#[test]
fn oversized_envelopes_are_refused_before_encryption() {
    let request = Request::new(
        Op::Deploy,
        "r",
        json!({"blob": "x".repeat(MAX_PLAINTEXT_BYTES)}),
    );
    assert_eq!(
        build_request(&agent(), &manager().public_key(), None, &request, now()).unwrap_err(),
        WireError::TooLarge
    );
}

/// Confidentiality rule 5: neither the envelopes' `Debug` nor any error's
/// `Display` carries payload content.
#[test]
fn debug_and_errors_never_show_payload_content() {
    let request = secret_request();
    let opened = open_request(&request_event(&request), &manager()).unwrap();
    let nsec = agent().secret_key().to_bech32().unwrap();
    for text in [
        format!("{request:?}"),
        format!("{opened:?}"),
        format!("{:?}", refusal()),
    ] {
        for needle in [nsec.as_str(), API_KEY, OAUTH_TOKEN, "session ended"] {
            assert!(!text.contains(needle), "{needle} in {text}");
        }
    }
    let error = WireError::UnsupportedOp {
        request_id: "secret-request-id".into(),
    };
    assert!(!error.to_string().contains("secret-request-id"));
    assert_eq!(error.label(), "unsupported_op");
}
