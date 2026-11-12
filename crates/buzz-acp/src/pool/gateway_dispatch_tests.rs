//! Mixed-author dispatch crosses the actual ACP prompt boundary.
use super::*;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, Keys, Kind};
use nostr::{Tag, Timestamp};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn refused_gateway_in_a_batch_cannot_dead_letter_owner_work() {
    let (owner, gateway) = (Keys::generate(), Keys::generate());
    let relay = crate::gateway::tests::fake_relay().await;
    let channel: Uuid = "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10".parse().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let capture = directory.path().join("wire.ndjson");
    let quoted = capture.to_string_lossy().replace('\'', "'\\''");
    let script = format!(
        r#"while IFS= read -r line; do
printf '%s\n' "$line" >> '{quoted}'
printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"stopReason":"end_turn"}}}}'
done"#
    );
    for row in 0..4 {
        let cancelled = row == 1;
        let coexist = row == 2;
        let interrupted_owner = row == 3;
        if capture.exists() {
            std::fs::remove_file(&capture).unwrap();
        }
        let acp = AcpClient::spawn("bash", &["-c".into(), script.clone()], &[], false)
            .await
            .unwrap();
        let scope = SessionScope::Thread {
            channel_id: channel,
            root_event_id: "a".repeat(64),
        };
        let mut state = SessionState::default();
        state
            .sessions
            .insert(scope.clone(), "existing-session".into());
        state
            .deliveries
            .insert(scope.clone(), ChannelDeliveryState::default());
        let agent = OwnedAgent {
            index: 0,
            acp,
            state,
            model_capabilities: None,
            desired_model: None,
            model_overridden: false,
            desired_model_request_id: None,
            desired_model_pending_ack: false,
            startup_effort: None,
            agent_name: "legacy-test-agent".into(),
            goose_system_prompt_supported: None,
            protocol_version: 1,
        };
        let mut ctx = super::tests::make_prompt_context_no_owner();
        let agent_pubkey = ctx.agent_keys.public_key();
        let event = |signer: &Keys, text| crate::queue::BatchEvent {
            event: EventBuilder::new(Kind::Custom(9), text)
                .tags([
                    Tag::public_key(agent_pubkey),
                    Tag::parse(["h", &channel.to_string()]).unwrap(),
                    Tag::parse(["e", &"a".repeat(64), "", "root"]).unwrap(),
                ])
                .sign_with_keys(signer)
                .unwrap(),
            prompt_tag: "@mention".into(),
            received_at: std::time::Instant::now(),
            edit: None,
        };
        let mut batch = FlushBatch {
            channel_id: channel,
            scope,
            events: vec![event(&owner, "OWNER_SURVIVES")],
            cancelled_events: vec![],
            cancel_reason: None,
        };
        let rejected = event(
            &gateway,
            if coexist {
                "LIVE_GATEWAY_WORK"
            } else {
                "REVOKED_GATEWAY_POISON"
            },
        );
        if interrupted_owner {
            batch.cancelled_events = std::mem::take(&mut batch.events);
            batch.events.push(rejected);
            batch.cancel_reason = Some(crate::queue::CancelReason::Steer);
        } else if cancelled {
            batch.cancelled_events.push(rejected);
        } else {
            batch.events.push(rejected);
        }
        ctx.agent_owner_pubkey = Some(owner.public_key());
        ctx.dedup_mode = DedupMode::Queue;
        let mut rest = ctx.rest_client.clone();
        rest.base_url = relay.rest.base_url.clone();
        *relay.heads.lock().unwrap() = vec![if coexist {
            crate::gateway::tests::grant(&owner, gateway.public_key(), 10)
        } else {
            crate::gateway::tests::revoke(&owner, gateway.public_key(), 5)
        }];
        *relay.members.lock().unwrap() = vec![
            owner.public_key(),
            ctx.agent_keys.public_key(),
            gateway.public_key(),
        ];
        let manager = Keys::generate();
        let teammate = Keys::generate();
        let mut sharing_task = None;
        if coexist {
            let (url, task) = sharing_manager(manager.clone(), teammate.public_key()).await;
            sharing_task = Some(task);
            let mut args = crate::config::CliArgs::try_parse_from([
                "buzz-acp",
                "--private-key",
                &ctx.agent_keys.secret_key().to_secret_hex(),
                "--agent-owner",
                &owner.public_key().to_hex(),
            ])
            .unwrap();
            args.sharing_manager = Some(manager.public_key());
            args.bound_session = Some(crate::config::BoundSession::Thread {
                channel_id: channel,
                thread_root: "a".repeat(64),
            });
            args.respond_to = crate::RespondTo::Owner;
            args.session_policy = crate::scope::SessionPolicy::Thread;
            args.heartbeat_interval = 0;
            args.relay_url = url;
            let config = crate::config::Config::from_args(args).unwrap();
            rest.auth_tag_json = Some(
                buzz_sdk::nip_oa::compute_auth_tag(&owner, &ctx.agent_keys.public_key(), "")
                    .unwrap(),
            );
            ctx.sharing = crate::sharing::Client::from_config(&config, &rest)
                .unwrap()
                .map(Arc::new);
            batch.events.push(event(&teammate, "TEAMMATE_SHARED_WORK"));
        }
        ctx.gateways = Some(Arc::new(crate::gateway::GatewayAccess::new(
            owner.public_key(),
            ctx.agent_keys.public_key(),
            rest,
            channel,
        )));
        let (tx, mut rx) = mpsc::unbounded_channel();
        timeout(
            Duration::from_secs(15),
            run_prompt_task(
                agent,
                Some(batch),
                None,
                Arc::new(ctx),
                tx,
                None,
                "mixed-author-turn".into(),
                Default::default(),
            ),
        )
        .await
        .unwrap();
        let mut result = rx.recv().await.unwrap();
        assert!(
            matches!(result.outcome, PromptOutcome::Ok(StopReason::EndTurn)),
            "owner request was refused with unrelated gateway work"
        );
        assert!(
            result.batch.is_none(),
            "owner message entered batch retry/dead-letter accounting"
        );
        result.agent.acp.shutdown().await;
        let wire = std::fs::read_to_string(&capture).unwrap();
        assert!(wire.contains("OWNER_SURVIVES"));
        assert!(!wire.contains("REVOKED_GATEWAY_POISON"));
        assert_eq!(wire.contains("LIVE_GATEWAY_WORK"), coexist);
        assert_eq!(wire.contains("TEAMMATE_SHARED_WORK"), coexist);
        if let Some(task) = sharing_task {
            timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
        }
    }
}

async fn sharing_manager(
    manager: Keys,
    teammate: nostr::PublicKey,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        ws.send(Message::Text(
            serde_json::json!(["AUTH", "shared-fixture"])
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
            match frame[0].as_str().unwrap() {
                "AUTH" => ws
                    .send(Message::Text(
                        serde_json::json!(["OK", frame[1]["id"], true, ""])
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap(),
                "REQ" => ws
                    .send(Message::Text(
                        serde_json::json!(["EOSE", "shared-reply"])
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap(),
                "EVENT" => {
                    let event: Event = serde_json::from_value(frame[1].clone()).unwrap();
                    let request = buzz_session_control::open_request(&event, &manager).unwrap();
                    assert_eq!(request.request.op, buzz_session_control::Op::Admit);
                    assert_eq!(
                        request.request.payload["trigger"]["pubkey"],
                        serde_json::json!(teammate)
                    );
                    let reply = buzz_session_control::build_reply(
                        &manager,
                        &event.pubkey,
                        &event.id,
                        &buzz_session_control::Reply::done(
                            request.request.request_id,
                            serde_json::json!({"ok":true,"human":teammate}),
                        ),
                        Timestamp::now(),
                    )
                    .unwrap();
                    ws.send(Message::Text(
                        serde_json::json!(["OK", event.id, true, ""])
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                    ws.send(Message::Text(
                        serde_json::json!(["EVENT", "shared-reply", reply])
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                    return;
                }
                other => panic!("unexpected {other}"),
            }
        }
    });
    (url, task)
}
