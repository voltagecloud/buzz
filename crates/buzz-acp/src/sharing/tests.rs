use super::*;
use futures_util::{SinkExt, StreamExt};
use nostr::{EventBuilder, Kind, Tag};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

fn fixture(relay: String, manager: PublicKey, directory: std::path::PathBuf) -> (Client, Event) {
    let owner = Keys::generate();
    let agent = Keys::generate();
    let channel = uuid::Uuid::new_v4();
    let event = EventBuilder::new(Kind::Custom(9), "team task")
        .tags([
            Tag::parse(["h", &channel.to_string()]).unwrap(),
            Tag::public_key(agent.public_key()),
        ])
        .sign_with_keys(&Keys::generate())
        .unwrap();
    let scope =
        buzz_sdk::sharing::task::scope(&event, agent.public_key(), Timestamp::now().as_secs())
            .unwrap();
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
    (
        Client {
            keys: agent,
            auth,
            relay,
            manager,
            scope,
            outbox: Arc::new(tokio::sync::Mutex::new(notices::Outbox::new(directory))),
        },
        event,
    )
}

#[tokio::test]
async fn encrypted_admission_uses_fresh_ids_and_request_scoped_reply_filters() {
    let directory = tempfile::tempdir().unwrap();
    let manager = Keys::generate();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = requests.clone();
    let manager_pub = manager.public_key();
    let human = Keys::generate().public_key();
    let server = tokio::spawn(async move {
        for _ in 0..5 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(json!(["AUTH", "fixture"]).to_string().into()))
                .await
                .unwrap();
            let mut subscribed = None;
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let message: serde_json::Value = serde_json::from_str(&text).unwrap();
                match message[0].as_str().unwrap() {
                    "AUTH" => socket
                        .send(Message::Text(
                            json!(["OK", message[1]["id"], true, ""]).to_string().into(),
                        ))
                        .await
                        .unwrap(),
                    "REQ" => {
                        subscribed = Some(message[2]["#e"][0].as_str().unwrap().to_owned());
                        socket
                            .send(Message::Text(
                                json!(["EOSE", "shared-reply"]).to_string().into(),
                            ))
                            .await
                            .unwrap();
                    }
                    "EVENT" => {
                        let event: Event = serde_json::from_value(message[1].clone()).unwrap();
                        if event.kind.as_u16() == 9 {
                            assert!(!event.tags.iter().any(|t| t
                                .as_slice()
                                .first()
                                .map(String::as_str)
                                == Some("p")));
                            socket
                                .send(Message::Text(
                                    json!(["OK", event.id, true, ""]).to_string().into(),
                                ))
                                .await
                                .unwrap();
                            break;
                        }
                        assert_eq!(subscribed.as_deref(), Some(event.id.to_hex().as_str()));
                        assert!(
                            serde_json::from_str::<serde_json::Value>(&event.content).is_err(),
                            "control content must be encrypted"
                        );
                        let opened = buzz_session_control::open_request(&event, &manager).unwrap();
                        assert_eq!(opened.request.op, Op::Admit);
                        let attempt = captured.lock().unwrap().len();
                        captured
                            .lock()
                            .unwrap()
                            .push(opened.request.request_id.clone());
                        socket
                            .send(Message::Text(
                                json!(["OK", event.id, true, ""]).to_string().into(),
                            ))
                            .await
                            .unwrap();
                        let response = match attempt {
                            0 => json!({"ok":true}),
                            3 => json!({"ok":true,"human":human}),
                            _ => {
                                json!({"ok":false,"code":"shared_task_denied","error":"sharing was revoked"})
                            }
                        };
                        let reply =
                            buzz_session_control::Reply::done(opened.request.request_id, response);
                        let event = buzz_session_control::build_reply(
                            &manager,
                            &opened.signer,
                            &event.id,
                            &reply,
                            Timestamp::now(),
                        )
                        .unwrap();
                        socket
                            .send(Message::Text(
                                json!(["EVENT", "shared-reply", event]).to_string().into(),
                            ))
                            .await
                            .unwrap();
                        break;
                    }
                    other => panic!("unexpected frame {other}"),
                }
            }
        }
    });
    let (client, event) = fixture(url, manager_pub, directory.path().into());
    assert_eq!(
        client.authorize(&event).await.unwrap(),
        Admission::Allowed(event.pubkey)
    );
    assert_eq!(
        client.authorize(&event).await.unwrap(),
        Admission::Refused("sharing was revoked".into())
    );
    let (rest, rest_server) =
        crate::author_gate_tests::nip11_server(json!({"self":manager_pub})).await;
    let channels = crate::pool::ChannelInfoResolver::new(
        std::collections::HashMap::from([(
            client.scope.channel,
            crate::relay::ChannelInfo {
                name: "shared".into(),
                channel_type: "stream".into(),
                description: None,
            },
        )]),
        rest.clone(),
    );
    let owner = buzz_sdk::nip_oa::verify_auth_tag(&client.auth, &client.keys.public_key()).unwrap();
    let owner = crate::OwnerCache::new(Some(owner.to_hex()));
    let mut gate =
        crate::InboundAuthorGate::connect(&rest, &client.keys.public_key().to_hex(), "shared-test")
            .await;
    gate.enable_sharing(Some(Arc::new(client.clone())));
    let inbound = || crate::relay::BuzzEvent {
        channel_id: client.scope.channel,
        connection_generation: 0,
        event: event.clone(),
    };
    assert!(crate::authorize_normal_listener_event(
        &mut gate,
        inbound(),
        &crate::config::RespondTo::Owner,
        &Default::default(),
        &owner,
        &channels,
        &rest
    )
    .await
    .unwrap()
    .is_none());
    let allowed = crate::authorize_normal_listener_event(
        &mut gate,
        inbound(),
        &crate::config::RespondTo::Owner,
        &Default::default(),
        &owner,
        &channels,
        &rest,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        allowed.into_parts().1,
        human.to_hex(),
        "a verified manager response supplies the workflow's authorizing human"
    );
    rest_server.abort();
    server.await.unwrap();
    let ids = requests.lock().unwrap();
    assert_eq!(ids.len(), 4);
    assert_ne!(
        ids[0], ids[1],
        "a cached transport approval must not survive revocation"
    );
}

#[test]
fn delegated_author_metadata_cannot_replace_a_handoff_signer_or_accept_malformed_humans() {
    let directory = tempfile::tempdir().unwrap();
    let (client, root) = fixture(
        "ws://127.0.0.1:1".into(),
        Keys::generate().public_key(),
        directory.path().into(),
    );
    let human = Keys::generate().public_key();
    assert_eq!(
        admitted_author(&json!({"human":human}), &root).unwrap(),
        human
    );
    for value in [
        json!(null),
        json!(42),
        json!("invalid"),
        json!(human.to_hex().to_uppercase()),
    ] {
        assert!(admitted_author(&json!({"human":value}), &root).is_err());
    }
    let handoff = buzz_sdk::sharing::task::handoff(
        &root,
        client.keys.public_key(),
        Keys::generate().public_key(),
        "continue",
        Timestamp::now().as_secs(),
    )
    .unwrap()
    .sign_with_keys(&client.keys)
    .unwrap();
    assert_eq!(
        admitted_author(&json!({"human":human}), &handoff).unwrap(),
        client.keys.public_key(),
        "a handoff must retain its actual agent signer"
    );
}

#[tokio::test]
async fn failed_refusal_delivery_is_durable_and_does_not_generate_agent_mentions() {
    let directory = tempfile::tempdir().unwrap();
    let (client, event) = fixture(
        "ws://127.0.0.1:1".into(),
        Keys::generate().public_key(),
        directory.path().into(),
    );
    client
        .refuse(&event, "authorization unavailable; mention me again")
        .await
        .unwrap();
    let outbox = notices::Outbox::new(directory.path().into());
    let (_, notice) = outbox
        .next(Timestamp::now().as_secs() + 60)
        .unwrap()
        .unwrap();
    assert_eq!(notice.trigger, event.id);
    assert_eq!(notice.attempts, 1);
    assert!(notice.event.verify().is_ok());
    assert!(!notice
        .event
        .tags
        .iter()
        .any(|t| t.as_slice().first().map(String::as_str) == Some("p")));
    assert!(notice.event.content.contains("authorization unavailable"));
    let outside = EventBuilder::new(Kind::Custom(9), "different thread")
        .tags([
            Tag::parse(["h", &client.scope.channel.to_string()]).unwrap(),
            Tag::public_key(client.keys.public_key()),
        ])
        .sign_with_keys(&Keys::generate())
        .unwrap();
    assert!(!client.accepts(&outside));
    assert!(client.refuse(&outside, "no").await.is_err());
}

#[test]
fn outbox_replays_exact_signatures_and_bounds_storage() {
    let directory = tempfile::tempdir().unwrap();
    let owner = Keys::generate();
    let event = EventBuilder::new(Kind::Custom(9), "refusal")
        .sign_with_keys(&owner)
        .unwrap();
    let outbox = notices::Outbox::new(directory.path().into());
    outbox.reserve(event.id, event.clone()).unwrap();
    let (path, mut notice) = outbox.next(0).unwrap().unwrap();
    outbox.attempt(&path, &mut notice, 0).unwrap();
    let recovered = notices::Outbox::new(directory.path().into());
    assert!(recovered.next(9).unwrap().is_none());
    let (_, retry) = recovered.next(10).unwrap().unwrap();
    assert_eq!(retry.event, event);
    recovered.complete(&path).unwrap();
    assert!(recovered.next(10).unwrap().is_none());
    let huge = EventBuilder::new(Kind::Custom(9), "x".repeat(5000))
        .sign_with_keys(&owner)
        .unwrap();
    assert!(outbox.reserve(huge.id, huge).is_err());
    for i in 0..32 {
        outbox
            .reserve(nostr::EventId::from_byte_array([i; 32]), event.clone())
            .unwrap();
    }
    assert!(outbox
        .reserve(nostr::EventId::from_byte_array([33; 32]), event)
        .is_err());
}

#[tokio::test]
async fn queued_shared_task_is_rechecked_before_context_or_acp_session_creation() {
    use crate::pool::{OwnedAgent, PromptOutcome, SessionState};
    let directory = tempfile::tempdir().unwrap();
    let (client, event) = fixture(
        "ws://127.0.0.1:1".into(),
        Keys::generate().public_key(),
        directory.path().join("notices"),
    );
    let capture = directory.path().join("acp-wire.ndjson");
    let quoted = capture.to_string_lossy().replace('\'', "'\\''");
    let script =
        format!("while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{quoted}'; done");
    let acp = crate::acp::AcpClient::spawn("bash", &["-c".into(), script], &[], false)
        .await
        .unwrap();
    let agent = OwnedAgent {
        index: 0,
        acp,
        state: SessionState::default(),
        model_capabilities: None,
        desired_model: None,
        model_overridden: false,
        desired_model_request_id: None,
        desired_model_pending_ack: false,
        startup_effort: None,
        agent_name: "sharing-boundary".into(),
        goose_system_prompt_supported: None,
        protocol_version: 1,
    };
    let mut ctx = crate::pool::tests::make_prompt_context_no_owner();
    ctx.agent_owner_pubkey =
        Some(buzz_sdk::nip_oa::verify_auth_tag(&client.auth, &client.keys.public_key()).unwrap());
    ctx.agent_keys = client.keys.clone();
    ctx.sharing = Some(Arc::new(client.clone()));
    ctx.memory_enabled = true;
    ctx.initial_message = Some("private setup context must not be sent".into());
    ctx.dedup_mode = crate::config::DedupMode::Queue;
    let batch = crate::queue::FlushBatch {
        channel_id: client.scope.channel,
        scope: crate::scope::SessionScope::Thread {
            channel_id: client.scope.channel,
            root_event_id: client.scope.root.to_hex(),
        },
        events: vec![crate::queue::BatchEvent {
            event: event.clone(),
            prompt_tag: "shared".into(),
            received_at: std::time::Instant::now(),
            edit: None,
        }],
        cancelled_events: vec![],
        cancel_reason: None,
    };
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(3),
        crate::pool::run_prompt_task(
            agent,
            Some(batch),
            None,
            Arc::new(ctx),
            send,
            None,
            "queued-shared-task".into(),
            Default::default(),
        ),
    )
    .await
    .unwrap();
    let mut result = receive.recv().await.unwrap();
    assert!(matches!(
        result.outcome,
        PromptOutcome::Error(crate::acp::AcpError::TaskPermission(_))
    ));
    assert_eq!(
        result.batch.take().unwrap().events[0].event.id,
        event.id,
        "retain the original task for bounded retry"
    );
    result.agent.acp.shutdown().await;
    assert!(
        !capture.exists(),
        "an indeterminate shared task must not reach memory or ACP session setup"
    );
}
