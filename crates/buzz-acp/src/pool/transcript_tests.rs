//! NIP-AT: a real turn through `run_prompt_task` publishes its transcript to
//! the owner and each live gateway, and never to a revoked gateway.
use super::*;
use buzz_sdk::transcript::open_agent_session_transcript;
use nostr::{EventBuilder, Keys, Kind, Tag};

/// A fake ACP agent: answers a `session/prompt` with chunks, a tool call, its
/// update, a usage report, a non-kept update, and the prompt response.
const AGENT_SCRIPT: &str = r##"while IFS= read -r line; do
id=$(printf '%s' "$line" | grep -o '"id":[0-9][0-9]*' | head -n1 | cut -d: -f2)
[ -z "$id" ] && continue
case "$line" in
*'"session/prompt"'*)
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"existing-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hel"}}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"existing-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"lo"}}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"existing-session","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Edit a.rs","kind":"edit","status":"pending","locations":[{"path":"/home/agent/repo/a.rs"}]}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"existing-session","update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"type":"diff","path":"/home/agent/repo/a.rs","oldText":"a","newText":"token=ghp_transcriptE2Esecret0123456789"}]}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"existing-session","update":{"sessionUpdate":"usage_update","size":200000,"used":4321}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"existing-session","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"echoed prompt"}}}}'
printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn","usage":{"inputTokens":12,"outputTokens":7,"totalTokens":19}}}\n' "$id"
;;
*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
esac
done"##;

/// Mutations: publish to the owner only (drop the gateway lookup) → the live
/// gateway's part is missing; skip the `live` filter (encrypt to every head)
/// → the revoked gateway receives a part; drop the recorder from `observe`
/// → the parts carry no updates; spawn the publish outside
/// `ctx.transcript_tasks` → nothing is published when the tracker (which
/// shutdown waits on) is idle; build the sink without `ctx.transcript_secrets`
/// → the injected token reaches the parts.
#[tokio::test]
async fn turn_transcript_reaches_owner_and_live_gateway_but_not_a_revoked_one() {
    let (owner, live, revoked) = (Keys::generate(), Keys::generate(), Keys::generate());
    let relay = crate::gateway::tests::fake_relay().await;
    let channel: Uuid = "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10".parse().unwrap();
    let root = "a".repeat(64);
    let workdir = tempfile::tempdir().unwrap();

    let acp = AcpClient::spawn("bash", &["-c".into(), AGENT_SCRIPT.into()], &[], false)
        .await
        .unwrap();
    let scope = SessionScope::Thread {
        channel_id: channel,
        root_event_id: root.clone(),
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
        agent_name: "transcript-test-agent".into(),
        goose_system_prompt_supported: None,
        protocol_version: 1,
    };

    let mut ctx = super::tests::make_prompt_context_no_owner();
    let agent_keys = ctx.agent_keys.clone();
    ctx.agent_owner_pubkey = Some(owner.public_key());
    ctx.cwd = workdir.path().to_string_lossy().into_owned();
    ctx.transcript_secrets = Arc::new(crate::transcript::Secrets::collect(
        None,
        [(
            "GITHUB_TOKEN".to_owned(),
            "ghp_transcriptE2Esecret0123456789".to_owned(),
        )],
    ));
    let transcript_tasks = ctx.transcript_tasks.clone();
    ctx.rest_client.base_url = relay.rest.base_url.clone();
    *relay.heads.lock().unwrap() = vec![
        crate::gateway::tests::grant(&owner, live.public_key(), 10),
        crate::gateway::tests::grant(&owner, revoked.public_key(), 20),
        crate::gateway::tests::revoke(&owner, revoked.public_key(), 5),
    ];
    *relay.members.lock().unwrap() = vec![
        owner.public_key(),
        agent_keys.public_key(),
        live.public_key(),
        revoked.public_key(),
    ];
    let mut gateway_rest = ctx.rest_client.clone();
    gateway_rest.base_url = relay.rest.base_url.clone();
    ctx.gateways = Some(Arc::new(crate::gateway::GatewayAccess::new(
        owner.public_key(),
        agent_keys.public_key(),
        gateway_rest,
        channel,
    )));

    let event = EventBuilder::new(Kind::Custom(9), "please edit a.rs")
        .tags([
            Tag::public_key(agent_keys.public_key()),
            Tag::parse(["h", &channel.to_string()]).unwrap(),
            Tag::parse(["e", &root, "", "root"]).unwrap(),
        ])
        .sign_with_keys(&owner)
        .unwrap();
    let batch = FlushBatch {
        channel_id: channel,
        scope,
        events: vec![crate::queue::BatchEvent {
            event,
            prompt_tag: "@mention".into(),
            received_at: std::time::Instant::now(),
            edit: None,
        }],
        cancelled_events: vec![],
        cancel_reason: None,
    };
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
            "transcript-turn".into(),
            Default::default(),
        ),
    )
    .await
    .unwrap();
    let mut result = rx.recv().await.unwrap();
    assert!(matches!(
        result.outcome,
        PromptOutcome::Ok(StopReason::EndTurn)
    ));
    result.agent.acp.shutdown().await;

    let transcripts = || -> Vec<nostr::Event> {
        relay
            .published
            .lock()
            .unwrap()
            .iter()
            .filter(|e| {
                e.kind == Kind::Custom(buzz_core::kind::KIND_AGENT_SESSION_TRANSCRIPT as u16)
            })
            .cloned()
            .collect()
    };
    // The publish is registered with the tracker shutdown drains, so once
    // the tracker is idle, every part is already on the relay.
    transcript_tasks.close();
    timeout(Duration::from_secs(10), transcript_tasks.wait())
        .await
        .expect("transcript publish settles");

    let parts = transcripts();
    let recipient = |event: &nostr::Event| {
        buzz_sdk::transcript::parse_envelope(event)
            .unwrap()
            .recipient
    };
    // Recipients are published in parallel, so their order is not fixed.
    let mut recipients: Vec<_> = parts.iter().map(recipient).collect();
    recipients.sort();
    let mut expected = vec![owner.public_key(), live.public_key()];
    expected.sort();
    assert_eq!(
        recipients, expected,
        "one part each for the owner and the live gateway; none for the revoked gateway"
    );
    for event in &parts {
        let reader = if recipient(event) == owner.public_key() {
            &owner
        } else {
            &live
        };
        let (envelope, payload) =
            open_agent_session_transcript(reader, &agent_keys.public_key(), event).unwrap();
        assert_eq!((envelope.part_index, envelope.part_count), (0, 1));
        assert_eq!(envelope.channel, channel);
        assert_eq!(envelope.turn_id, "transcript-turn");
        assert_eq!(payload.thread_root, root);
        assert_eq!(payload.stop_reason, "end_turn");
        assert_eq!(payload.usage.as_ref().unwrap().total_tokens, Some(19));
        assert_eq!(payload.context.unwrap().used, 4321);
        assert!(payload.workspace.is_empty());
        let kinds: Vec<&str> = payload
            .updates
            .iter()
            .map(|u| u["sessionUpdate"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["agent_message_chunk", "tool_call", "tool_call_update"]
        );
        assert_eq!(payload.updates[0]["content"]["text"], "Hello");
        assert_eq!(
            payload.updates[1]["locations"][0]["path"],
            "/home/agent/repo/a.rs"
        );
        // The token the harness knows is redacted from the tool's diff.
        let diff = &payload.updates[2]["content"][0];
        assert_eq!(diff["newText"], "token=[redacted]");
        // The revoked gateway cannot open the reader's part either.
        assert!(open_agent_session_transcript(&revoked, &agent_keys.public_key(), event).is_err());
    }
}
