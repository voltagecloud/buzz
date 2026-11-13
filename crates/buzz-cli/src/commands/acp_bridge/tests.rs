//! acp-bridge tests. The journey drives the production bridge loop over
//! in-memory stdio with only the relay boundary faked; the focused tests
//! cover frame acceptance and mapping tables the journey cannot enumerate.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use buzz_core::observer::{decrypt_observer_payload, encrypt_observer_payload};
use buzz_sdk::ThreadRef;
use nostr::{Event, EventBuilder, EventId, Keys, Kind, PublicKey, Tag};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio::sync::{mpsc, Notify, Semaphore};
use uuid::Uuid;

use super::bridge::{run_bridge, BridgeRelay, RelayInbound, Timing, DEFERRED_REPLY_CAP};
use super::frames::{cancel_turn_frame, decode_telemetry, signal_of, FrameError, Scope, Signal};
use super::require_wss;
use crate::error::CliError;

const WAIT: Duration = Duration::from_secs(5);

struct Ids {
    signer: Keys,
    agent: Keys,
    channel: Uuid,
    /// The bridge's home directory, removed when the test ends.
    home: tempfile::TempDir,
}

impl Ids {
    fn new() -> Self {
        Self {
            signer: Keys::generate(),
            agent: Keys::generate(),
            channel: Uuid::new_v4(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn scope(&self) -> Scope {
        Scope {
            signer: self.signer.clone(),
            owner: self.signer.public_key(),
            agent: self.agent.public_key(),
            channel: self.channel,
            home: self.home.path().to_path_buf(),
            paseo_code: self.home.path().join("bin/paseo-code"),
        }
    }

    /// A telemetry frame from the agent to the signer.
    fn telemetry(&self, plaintext: &Value) -> Event {
        telemetry_from(
            &self.agent,
            &self.agent.public_key(),
            &self.signer.public_key(),
            plaintext,
        )
    }
}

fn telemetry_from(sender: &Keys, agent: &PublicKey, owner: &PublicKey, plaintext: &Value) -> Event {
    let encrypted = encrypt_observer_payload(sender, owner, plaintext).unwrap();
    buzz_sdk::build_agent_observer_frame(&owner.to_hex(), &agent.to_hex(), "telemetry", &encrypted)
        .unwrap()
        .sign_with_keys(sender)
        .unwrap()
}

/// An `ObserverEvent` as buzz-acp serializes it.
fn observer(kind: &str, channel: Option<Uuid>, turn: Option<&str>, payload: Value) -> Value {
    json!({
        "seq": 1,
        "timestamp": "2026-11-09T00:00:00Z",
        "kind": kind,
        "agentIndex": 0,
        "channelId": channel.map(|c| c.to_string()),
        "sessionId": "adapter-session",
        "turnId": turn,
        "payload": payload,
    })
}

fn acp_update(update: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "adapter-session", "update": update}})
}

fn chunk(text: &str) -> Value {
    acp_update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
    )
}

fn thread_message(author: &Keys, channel: Uuid, root: &str, content: &str) -> Event {
    thread_reply(author, channel, root, root, content)
}

/// A thread message replying to `parent`, as the sandbox manager's startup
/// notices reply to the mention that triggered them.
fn thread_reply(author: &Keys, channel: Uuid, root: &str, parent: &str, content: &str) -> Event {
    let thread = ThreadRef {
        root_event_id: EventId::from_hex(root).unwrap(),
        parent_event_id: EventId::from_hex(parent).unwrap(),
    };
    buzz_sdk::build_message(channel, content, Some(&thread), &[], false, &[], &[])
        .unwrap()
        .sign_with_keys(author)
        .unwrap()
}

fn tag<'a>(event: &'a Event, name: &str) -> Vec<&'a [String]> {
    event
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|parts| parts.first().map(String::as_str) == Some(name))
        .collect()
}

/// Relay boundary fake: builds prompts with the SDK builder, records
/// submissions and ephemeral publishes, and echoes stored events back
/// through the subscription like the relay does.
struct FakeRelay {
    query_count: AtomicUsize,
    query_filters: std::sync::Mutex<Vec<Value>>,
    scope: Scope,
    submitted: mpsc::UnboundedSender<Event>,
    ephemeral: mpsc::UnboundedSender<Event>,
    echo: mpsc::Sender<RelayInbound>,
    history: Arc<std::sync::Mutex<(Vec<Event>, bool)>>,
    query_gate: Arc<Semaphore>,
    query_entered: Arc<Notify>,
    consume_query_permits: Arc<AtomicBool>,
    /// Holds message preparation while a test keeps its permits.
    prepare_gate: Semaphore,
    /// Texts whose preparation started.
    prepared: std::sync::Mutex<Vec<String>>,
    /// Holds submission while a test keeps its permits.
    submit_gate: Semaphore,
    /// The next submission's failure, if any.
    submit_failure: std::sync::Mutex<Option<CliError>>,
    /// Submissions started.
    submits: AtomicUsize,
}

/// Permits of [`FakeRelay::prepare_gate`]; a test that takes them all holds
/// every preparation.
const PREPARE_GATE: u32 = 1 << 16;

fn fake_query_page(events: &[Event], filter: &Value) -> Vec<Event> {
    // Match the relay's SQL candidates, ordering and LIMIT first.
    // In particular kind 9 #d tags are only a later Rust post-filter.
    let tagged = |event: &Event, field: &str| {
        filter
            .get(field)
            .and_then(Value::as_array)
            .is_none_or(|values| {
                tag(event, &field[1..]).iter().any(|parts| {
                    parts
                        .get(1)
                        .is_some_and(|value| values.contains(&json!(value)))
                })
            })
    };
    let nip33 = filter["kinds"].as_array().is_some_and(|kinds| {
        !kinds.is_empty()
            && kinds.iter().all(|kind| {
                kind.as_u64()
                    .is_some_and(|kind| (30000..40000).contains(&kind))
            })
    });
    let single = |field: &str| {
        filter[field]
            .as_array()
            .is_some_and(|values| values.len() == 1)
    };
    let mut page: Vec<_> = events
        .iter()
        .filter(|event| {
            let ids = filter.get("ids").and_then(Value::as_array);
            ids.is_none_or(|ids| ids.contains(&json!(event.id.to_hex())))
                && filter["kinds"]
                    .as_array()
                    .is_none_or(|kinds| kinds.contains(&json!(event.kind.as_u16())))
                && filter["authors"]
                    .as_array()
                    .is_none_or(|authors| authors.contains(&json!(event.pubkey.to_hex())))
                && (!single("#h") || tagged(event, "#h"))
                && (!single("#p") || tagged(event, "#p"))
                && tagged(event, "#e")
                && (!nip33 || tagged(event, "#d"))
                && filter
                    .get("since")
                    .and_then(Value::as_u64)
                    .is_none_or(|since| event.created_at.as_secs() >= since)
                && filter
                    .get("until")
                    .and_then(Value::as_u64)
                    .is_none_or(|until| {
                        event.created_at.as_secs() < until
                            || (event.created_at.as_secs() == until
                                && filter
                                    .get("before_id")
                                    .and_then(Value::as_str)
                                    .is_none_or(|before| event.id.to_hex().as_str() > before))
                    })
        })
        .cloned()
        .collect();
    page.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    page.truncate(filter.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize);
    page.retain(|event| {
        tagged(event, "#d") && tagged(event, "#h") && tagged(event, "#p") && tagged(event, "#e")
    });
    page
}

impl BridgeRelay for FakeRelay {
    async fn query(&self, filter: Value) -> Result<Vec<Event>, CliError> {
        self.query_count.fetch_add(1, Ordering::SeqCst);
        self.query_filters.lock().unwrap().push(filter.clone());
        let page = {
            let state = self.history.lock().unwrap();
            if state.1 {
                return Err(CliError::Other("fixture history unavailable".into()));
            }
            match filter.as_array() {
                Some(filters) => filters
                    .iter()
                    .flat_map(|filter| fake_query_page(&state.0, filter))
                    .collect(),
                None => fake_query_page(&state.0, &filter),
            }
        };
        self.query_entered.notify_one();
        let guard = self.query_gate.acquire().await.unwrap();
        if self.consume_query_permits.load(Ordering::SeqCst) {
            guard.forget();
        }
        Ok(page)
    }

    async fn prepare_prompt(
        &self,
        content: String,
        thread_root: Option<String>,
    ) -> Result<Event, CliError> {
        self.prepared.lock().unwrap().push(content.clone());
        drop(self.prepare_gate.acquire().await.unwrap());
        let thread = thread_root.map(|root| {
            let root = EventId::from_hex(&root).unwrap();
            ThreadRef {
                root_event_id: root,
                parent_event_id: root,
            }
        });
        let agent = self.scope.agent.to_hex();
        buzz_sdk::build_message(
            self.scope.channel,
            &content,
            thread.as_ref(),
            &[agent.as_str()],
            false,
            &[],
            &[],
        )
        .map_err(|e| CliError::Other(e.to_string()))?
        .sign_with_keys(&self.scope.signer)
        .map_err(|e| CliError::Other(e.to_string()))
    }

    async fn submit(&self, event: Event) -> Result<(), CliError> {
        self.submits.fetch_add(1, Ordering::SeqCst);
        drop(self.submit_gate.acquire().await.unwrap());
        if let Some(error) = self.submit_failure.lock().unwrap().take() {
            return Err(error);
        }
        let _ = self.submitted.send(event.clone());
        let _ = self.echo.send(RelayInbound::Event(Box::new(event))).await;
        Ok(())
    }

    async fn publish_ephemeral(&self, event: Event) -> Result<(), CliError> {
        let _ = self.ephemeral.send(event);
        Ok(())
    }
}

/// The ACP client side of the in-memory stdio pipes.
struct Client {
    stdin: DuplexStream,
    stdout: Lines<BufReader<DuplexStream>>,
}

impl Client {
    async fn send(&mut self, frame: Value) {
        let mut line = frame.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
    }

    /// Read frames until the response to `id`, returning it and the
    /// notifications that arrived first.
    async fn response(&mut self, id: u64) -> (Value, Vec<Value>) {
        let mut notes = Vec::new();
        loop {
            let line = tokio::time::timeout(WAIT, self.stdout.next_line())
                .await
                .expect("bridge answered in time")
                .unwrap()
                .expect("bridge stdout open");
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("id") == Some(&json!(id)) {
                return (frame, notes);
            }
            notes.push(frame);
        }
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> (Value, Vec<Value>) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.response(id).await
    }
}

fn prompt(id: u64, session: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {"sessionId": session, "prompt": [{"type": "text", "text": text}]},
    })
}

/// The `crates/buzz-sandbox-reaper` mention launcher's reply to a mention
/// of an ended thread session it could not restore.
const ENDED_NOTICE: &str = "This thread's agent session ended. My owner, or a gateway they delegated, can restore its last checkpoint by mentioning me here; otherwise mention me in a new thread.";

/// The sandbox manager's ended-session notices: whether each invites a
/// mention in the thread (so `/recover` is offered) or only a new thread.
#[test]
fn ended_notices_offer_recover_only_when_a_mention_here_may_restore() {
    use super::bridge::mention_may_restore;
    for (reason, recover) in [
        // session_ended / recovery_interrupted
        (ENDED_NOTICE, true),
        // session_ending
        ("This thread's agent session ended and is still saving its checkpoint. Once that finishes, my owner or a gateway they delegated can restore it by mentioning me here; otherwise mention me in a new thread.", true),
        // recovery_mode_conflict
        ("This thread's agent session ended, and another recovery of it is in progress, so its last checkpoint could not be restored. Mention me here again once that recovery finishes, or mention me in a new thread.", true),
        // no_checkpoint
        (NO_CHECKPOINT_NOTICE, false),
        // A manager without mention recovery (before #187).
        ("This thread's agent session ended. Recover it from desktop, or mention me in a new thread.", false),
        ("This thread's agent session ended", false),
    ] {
        assert_eq!(mention_may_restore(reason), recover, "{reason}");
    }
}

/// The `crates/buzz-sandbox-reaper` notice for an ended thread with no
/// saved checkpoint.
const NO_CHECKPOINT_NOTICE: &str = "This thread's agent session ended without a saved checkpoint, so it cannot be restored. Mention me in a new thread.";

/// Names of the slash commands in the latest `available_commands_update`.
fn advertised(notes: &[Value]) -> Option<Vec<String>> {
    notes
        .iter()
        .rev()
        .find(|n| n["params"]["update"]["sessionUpdate"] == "available_commands_update")
        .map(|n| {
            n["params"]["update"]["availableCommands"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["name"].as_str().unwrap().to_owned())
                .collect()
        })
}

fn texts(notes: &[Value]) -> Vec<String> {
    notes
        .iter()
        .filter_map(|n| n["params"]["update"]["content"]["text"].as_str())
        .map(str::to_owned)
        .collect()
}

async fn recv<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("relay call in time")
        .expect("relay channel open")
}

struct Harness {
    relay: Arc<FakeRelay>,
    client: Client,
    relay_tx: mpsc::Sender<RelayInbound>,
    submitted: mpsc::UnboundedReceiver<Event>,
    ephemeral: mpsc::UnboundedReceiver<Event>,
    bridge: tokio::task::JoinHandle<Result<(), CliError>>,
    history: Arc<std::sync::Mutex<(Vec<Event>, bool)>>,
    query_gate: Arc<Semaphore>,
    query_entered: Arc<Notify>,
    consume_query_permits: Arc<AtomicBool>,
}

fn start(ids: &Ids, timing: Timing) -> Harness {
    start_scope(ids.scope(), timing)
}

fn start_scope(scope: Scope, timing: Timing) -> Harness {
    let (relay_tx, relay_rx) = mpsc::channel(64);
    let (submitted_tx, submitted) = mpsc::unbounded_channel();
    let (ephemeral_tx, ephemeral) = mpsc::unbounded_channel();
    let history = Arc::new(std::sync::Mutex::new((Vec::new(), false)));
    let query_gate = Arc::new(Semaphore::new(4));
    let query_entered = Arc::new(Notify::new());
    let consume_query_permits = Arc::new(AtomicBool::new(false));
    let relay = Arc::new(FakeRelay {
        query_count: AtomicUsize::new(0),
        query_filters: std::sync::Mutex::new(Vec::new()),
        scope: scope.clone(),
        submitted: submitted_tx,
        ephemeral: ephemeral_tx,
        echo: relay_tx.clone(),
        history: history.clone(),
        query_gate: query_gate.clone(),
        query_entered: query_entered.clone(),
        consume_query_permits: consume_query_permits.clone(),
        prepare_gate: Semaphore::new(PREPARE_GATE as usize),
        prepared: std::sync::Mutex::new(Vec::new()),
        submit_gate: Semaphore::new(PREPARE_GATE as usize),
        submit_failure: std::sync::Mutex::new(None),
        submits: AtomicUsize::new(0),
    });
    let (stdin, bridge_in) = tokio::io::duplex(1 << 16);
    let (bridge_out, stdout) = tokio::io::duplex(1 << 16);
    let bridge = tokio::spawn(run_bridge(
        scope,
        relay.clone(),
        relay_rx,
        BufReader::new(bridge_in),
        bridge_out,
        timing,
    ));
    Harness {
        relay,
        client: Client {
            stdin,
            stdout: BufReader::new(stdout).lines(),
        },
        relay_tx,
        submitted,
        ephemeral,
        bridge,
        history,
        query_gate,
        query_entered,
        consume_query_permits,
    }
}

impl Harness {
    async fn inject(&self, event: Event) {
        self.relay_tx
            .send(RelayInbound::Event(Box::new(event)))
            .await
            .unwrap();
    }
}

/// One ACP session through every turn shape: telemetry-driven turns with
/// fencing and dedup, cancel, the no-telemetry fallback, and an ended
/// sandbox session.
#[tokio::test]
async fn acp_session_drives_a_remote_agent_through_the_relay() {
    let ids = Ids::new();
    let timing = Timing {
        no_telemetry_grace: Duration::from_millis(300),
        cancel_wait: WAIT,
        turn_silence: Duration::from_secs(60),
        live_reply_delay: Duration::from_secs(60),
    };
    let mut h = start(&ids, timing);
    let channel = ids.channel;
    let agent_hex = ids.agent.public_key().to_hex();

    let (init, _) = h
        .client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], 1);
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);
    let (new, _) = h
        .client
        .request(2, "session/new", json!({"cwd": "/tmp", "mcpServers": []}))
        .await;
    let session = new["result"]["sessionId"].as_str().unwrap().to_owned();

    // Turn 1. The prompt waits until the subscription is live, so no early
    // telemetry can be missed, then posts a thread root mentioning the agent.
    h.client.send(prompt(3, &session, "hello agent")).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), h.submitted.recv())
            .await
            .is_err(),
        "prompt must not post before the relay subscription is live"
    );
    h.relay_tx.send(RelayInbound::Live).await.unwrap();
    let root = recv(&mut h.submitted).await;
    let root_id = root.id.to_hex();
    assert_eq!(root.kind, Kind::Custom(9));
    assert_eq!(root.content, "hello agent");
    assert_eq!(
        tag(&root, "h"),
        vec![&["h".to_owned(), channel.to_string()][..]]
    );
    assert!(tag(&root, "p").iter().any(|p| p[1] == agent_hex));
    assert!(
        tag(&root, "e").is_empty(),
        "the first prompt is a thread root"
    );

    // A turn for some other trigger, and its chunk: fenced out.
    h.inject(ids.telemetry(&observer(
        "turn_started",
        Some(channel),
        Some("stale"),
        json!({"triggeringEventIds": ["0".repeat(64)]}),
    )))
    .await;
    h.inject(ids.telemetry(&observer(
        "acp_read",
        Some(channel),
        Some("stale"),
        chunk("STALE"),
    )))
    .await;
    // Our turn. A kind 9 copy of the streamed text races ahead of it; it is
    // held, then dropped because the turn streamed the same text.
    h.inject(ids.telemetry(&observer(
        "turn_started",
        Some(channel),
        Some("t1"),
        json!({"triggeringEventIds": [root_id], "threadRootEventId": root_id}),
    )))
    .await;
    h.inject(thread_message(&ids.agent, channel, &root_id, "Hi there\n"))
        .await;
    // A batch envelope with forwarded and dropped updates.
    h.inject(ids.telemetry(&observer(
        "batch",
        Some(channel),
        Some("t1"),
        json!({"events": [
            observer("acp_read", Some(channel), Some("t1"), chunk("Hi there")),
            observer("acp_read", Some(channel), Some("t1"), acp_update(json!({
                "sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "Read file", "status": "pending",
            }))),
            observer("acp_read", Some(channel), Some("t1"), acp_update(json!({
                "sessionUpdate": "available_commands_update", "availableCommands": [],
            }))),
            // Context usage: forwarded with only its known fields.
            observer("acp_read", Some(channel), Some("t1"), acp_update(json!({
                "sessionUpdate": "usage_update", "size": 200_000, "used": 1_234,
                "cost": {"amount": 0.25, "currency": "USD", "note": "x"}, "_meta": {"leak": true},
            }))),
            // Without a context window size the meter cannot use it: dropped.
            observer("acp_read", Some(channel), Some("t1"), acp_update(json!({
                "sessionUpdate": "usage_update", "size": 0, "used": 5,
            }))),
        ]}),
    )))
    .await;
    // Same turn id, wrong channel: dropped.
    let other_channel = Uuid::new_v4();
    h.inject(ids.telemetry(&observer(
        "acp_read",
        Some(other_channel),
        Some("t1"),
        chunk("LEAK"),
    )))
    .await;
    // Tagged as the agent but signed by someone else: dropped.
    let impostor = Keys::generate();
    h.inject(telemetry_from(
        &impostor,
        &ids.agent.public_key(),
        &ids.signer.public_key(),
        &observer("acp_read", Some(channel), Some("t1"), chunk("IMPOSTOR")),
    ))
    .await;
    // The agent posts its answer with `buzz messages send`, so it differs
    // from the streamed narration and is shown when the turn ends.
    h.inject(thread_message(
        &ids.agent,
        channel,
        &root_id,
        "The answer is 42.",
    ))
    .await;
    h.inject(ids.telemetry(&observer(
        "acp_read",
        Some(channel),
        Some("t1"),
        json!({"jsonrpc": "2.0", "id": 7, "result": {"stopReason": "max_tokens", "usage": {
            "inputTokens": 900, "outputTokens": 100, "totalTokens": 1_000,
            "cachedReadTokens": 300, "thoughtTokens": -1, "secret": 7,
        }}}),
    )))
    .await;
    h.inject(ids.telemetry(&observer(
        "turn_completed",
        Some(channel),
        Some("t1"),
        json!({"duration_ms": 5}),
    )))
    .await;

    let (done, notes) = h.client.response(3).await;
    assert_eq!(done["result"]["stopReason"], "max_tokens");
    assert_eq!(
        done["result"]["usage"],
        json!({"inputTokens": 900, "outputTokens": 100, "totalTokens": 1_000, "cachedReadTokens": 300})
    );
    let usage: Vec<&Value> = notes
        .iter()
        .map(|n| &n["params"]["update"])
        .filter(|u| u["sessionUpdate"] == "usage_update")
        .collect();
    assert_eq!(
        usage,
        vec![&json!({
            "sessionUpdate": "usage_update", "size": 200_000, "used": 1_234,
            "cost": {"amount": 0.25, "currency": "USD"},
        })]
    );
    assert!(notes
        .iter()
        .all(|n| n["method"] == "session/update" && n["params"]["sessionId"] == session));
    assert_eq!(texts(&notes), vec!["Hi there", "The answer is 42."]);
    let kinds: Vec<&str> = notes
        .iter()
        .filter_map(|n| n["params"]["update"]["sessionUpdate"].as_str())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "agent_message_chunk",
            "tool_call",
            "usage_update",
            "agent_message_chunk"
        ]
    );

    // Turn 2: a reply in the same thread, then a cancel.
    h.client.send(prompt(4, &session, "second")).await;
    let reply = recv(&mut h.submitted).await;
    let markers = buzz_core::nip10::parse_thread_markers(&reply.tags);
    assert_eq!(markers.resolve(), Some((root_id.clone(), root_id.clone())));
    assert!(tag(&reply, "p").iter().any(|p| p[1] == agent_hex));
    assert_eq!(tag(&reply, "h")[0][1], channel.to_string());
    h.inject(ids.telemetry(&observer(
        "turn_started",
        Some(channel),
        Some("t2"),
        json!({"triggeringEventIds": [reply.id.to_hex()]}),
    )))
    .await;
    // No text streams this turn, so the agent's reply is shown when it ends.
    h.inject(thread_message(
        &ids.agent,
        channel,
        &root_id,
        "partial answer",
    ))
    .await;
    h.client
        .send(
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}}),
        )
        .await;
    let control = recv(&mut h.ephemeral).await;
    assert_eq!(control.pubkey, ids.signer.public_key());
    assert_eq!(control.kind, Kind::Custom(24200));
    assert_eq!(tag(&control, "p")[0][1], agent_hex);
    assert_eq!(tag(&control, "agent")[0][1], agent_hex);
    assert_eq!(tag(&control, "frame")[0][1], "control");
    let skew = control
        .created_at
        .as_secs()
        .abs_diff(nostr::Timestamp::now().as_secs());
    assert!(
        skew < 300,
        "the harness drops control frames older than 5 minutes"
    );
    let cancel: Value = decrypt_observer_payload(&ids.agent, &control).unwrap();
    assert_eq!(cancel["type"], "cancel_turn");
    assert_eq!(cancel["channelId"], channel.to_string());
    let request_id = cancel["requestId"].as_str().unwrap().to_owned();
    h.inject(ids.telemetry(&observer(
        "control_result",
        Some(channel),
        None,
        json!({"type": "cancel_turn", "status": "sent", "requestId": request_id}),
    )))
    .await;
    h.inject(ids.telemetry(&observer(
        "turn_completed",
        Some(channel),
        Some("t2"),
        json!({}),
    )))
    .await;
    let (done, notes) = h.client.response(4).await;
    assert_eq!(done["result"]["stopReason"], "cancelled");
    // Turn 1's usage belongs to turn 1; this turn reported none.
    assert!(done["result"].get("usage").is_none());
    assert_eq!(texts(&notes), vec!["partial answer"]);

    // Turn 3 without telemetry: a teammate's message is attributed, and the
    // agent's kind 9 reply is the answer and ends the turn after the grace.
    h.client.send(prompt(5, &session, "third")).await;
    recv(&mut h.submitted).await;
    let teammate = Keys::generate();
    h.inject(thread_message(
        &teammate,
        channel,
        &root_id,
        "teammate note",
    ))
    .await;
    h.inject(thread_message(
        &ids.agent,
        channel,
        &root_id,
        "fallback answer",
    ))
    .await;
    let (done, notes) = h.client.response(5).await;
    assert_eq!(done["result"]["stopReason"], "end_turn");
    let shown = texts(&notes);
    assert_eq!(shown.len(), 2, "{shown:?}");
    assert!(shown.iter().any(|text| text.contains("teammate note")
        && text.contains(&teammate.public_key().to_hex()[..8])));
    // A first profile lookup runs independently of the agent's reply.
    assert!(shown.iter().any(|text| text == "fallback answer"));

    // Turn 4: the sandbox manager reports the thread session ended. The
    // prompt ends normally and offers the ways to continue.
    h.client.send(prompt(6, &session, "fourth")).await;
    let fourth = recv(&mut h.submitted).await;
    h.inject(thread_reply(
        &ids.agent,
        channel,
        &root_id,
        &fourth.id.to_hex(),
        ENDED_NOTICE,
    ))
    .await;
    let (ended, notes) = h.client.response(6).await;
    assert_eq!(ended["result"]["stopReason"], "end_turn");
    let shown = texts(&notes).join("");
    assert!(
        shown.contains("/fresh")
            && shown.contains("/recover")
            && shown.contains("its manager can try to restore the session")
            && !shown.contains("restores the thread's last checkpoint")
            && shown.contains(&format!("> {ENDED_NOTICE}"))
            && shown.contains(&format!("buzz://message?channel={channel}&id={root_id}")),
        "{shown}"
    );
    assert_eq!(
        advertised(&notes),
        Some(vec!["recover".to_owned(), "fresh".to_owned()])
    );

    // Ordinary text is not posted to the ended session; the choices repeat.
    let (again, notes) = h
        .client
        .request(
            7,
            "session/prompt",
            prompt(7, &session, "hello?")["params"].clone(),
        )
        .await;
    assert_eq!(again["result"]["stopReason"], "end_turn");
    assert!(texts(&notes).join("").contains("/recover"));
    assert!(h.submitted.try_recv().is_err());

    // /recover resends the unanswered message in the same thread. A late
    // copy of the earlier notice is stale and does not end the retry.
    h.client.send(prompt(8, &session, "/recover")).await;
    let retry = recv(&mut h.submitted).await;
    assert_eq!(retry.content, "fourth");
    assert_ne!(retry.id, fourth.id, "a resend must be a new event");
    assert_eq!(
        buzz_core::nip10::parse_thread_markers(&retry.tags).resolve(),
        Some((root_id.clone(), root_id.clone()))
    );
    assert!(tag(&retry, "p").iter().any(|p| p[1] == agent_hex));
    h.inject(thread_reply(
        &ids.agent,
        channel,
        &root_id,
        &fourth.id.to_hex(),
        &format!("{ENDED_NOTICE} "),
    ))
    .await;
    // Relay events are handled in order: once this note shows, the stale
    // notice was handled and the retry is still running.
    h.inject(thread_message(&teammate, channel, &root_id, "sync"))
        .await;
    loop {
        let line = tokio::time::timeout(WAIT, h.client.stdout.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!line.contains("has ended"), "stale notice acted on: {line}");
        assert!(!line.contains("\"id\":8"), "retry ended early: {line}");
        if line.contains("sync") {
            break;
        }
    }
    // The manager could not restore the checkpoint: it refuses the retry too.
    h.inject(thread_reply(
        &ids.agent,
        channel,
        &root_id,
        &retry.id.to_hex(),
        ENDED_NOTICE,
    ))
    .await;
    let (ended, notes) = h.client.response(8).await;
    assert_eq!(ended["result"]["stopReason"], "end_turn");
    assert_eq!(advertised(&notes).map(|c| c.len()), Some(2));

    // /fresh resends it as a new thread root, linked to the ended thread.
    h.client.send(prompt(9, &session, "/fresh")).await;
    let first_fresh = recv(&mut h.submitted).await;
    let first_fresh_id = first_fresh.id.to_hex();
    assert!(first_fresh
        .content
        .contains(&format!("buzz://message?channel={channel}&id={root_id}")));
    // That thread ends at once too. Another /fresh resends the user's own
    // text with one link, to the thread that just ended: the link added to
    // the posted copy is not part of what the user wrote.
    h.inject(thread_reply(
        &ids.agent,
        channel,
        &first_fresh_id,
        &first_fresh_id,
        ENDED_NOTICE,
    ))
    .await;
    let (ended, _) = h.client.response(9).await;
    assert_eq!(ended["result"]["stopReason"], "end_turn");
    h.client.send(prompt(10, &session, "/fresh")).await;
    let fresh = recv(&mut h.submitted).await;
    let fresh_id = fresh.id.to_hex();
    assert!(tag(&fresh, "e").is_empty(), "/fresh posts a thread root");
    assert!(tag(&fresh, "p").iter().any(|p| p[1] == agent_hex));
    assert!(fresh.content.starts_with("fourth"), "{}", fresh.content);
    assert_eq!(
        fresh.content.matches("Previous thread").count(),
        1,
        "{}",
        fresh.content
    );
    assert!(fresh.content.contains(&format!(
        "buzz://message?channel={channel}&id={first_fresh_id}"
    )));
    // The old thread no longer belongs to the session.
    h.inject(thread_message(&teammate, channel, &root_id, "old thread"))
        .await;
    h.inject(ids.telemetry(&observer(
        "turn_started",
        Some(channel),
        Some("t5"),
        json!({"triggeringEventIds": [fresh_id]}),
    )))
    .await;
    h.inject(ids.telemetry(&observer(
        "acp_read",
        Some(channel),
        Some("t5"),
        chunk("fresh start"),
    )))
    .await;
    h.inject(ids.telemetry(&observer(
        "turn_completed",
        Some(channel),
        Some("t5"),
        json!({}),
    )))
    .await;
    let (done, notes) = h.client.response(10).await;
    assert_eq!(done["result"]["stopReason"], "end_turn");
    assert_eq!(texts(&notes), vec!["fresh start"]);
    assert_eq!(advertised(&notes), Some(Vec::new()), "commands withdrawn");

    // A kind 9 copy of the streamed text arriving after the turn ended is
    // still dropped; a different late reply is shown.
    h.inject(thread_message(
        &ids.agent,
        channel,
        &fresh_id,
        "fresh start",
    ))
    .await;
    h.inject(thread_message(
        &ids.agent,
        channel,
        &fresh_id,
        "late answer",
    ))
    .await;
    let line = tokio::time::timeout(WAIT, h.client.stdout.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(line.contains("late answer"), "{line}");

    // Closing stdin ends the bridge cleanly.
    drop(h.client);
    let result = tokio::time::timeout(WAIT, h.bridge).await.unwrap().unwrap();
    assert!(result.is_ok(), "{result:?}");
}

/// More prompts than the four preparation slots wait for a slot instead of
/// failing, for example when sessions queued while the relay was down.
/// Prompts cancelled while they wait never prepare, so repeated prompts and
/// cancels cannot pile up uploads.
#[tokio::test]
async fn prompts_beyond_the_preparation_slots_wait_instead_of_failing() {
    for cancel in [false, true] {
        let ids = Ids::new();
        let mut h = start(&ids, Timing::default());
        h.relay_tx.send(RelayInbound::Live).await.unwrap();
        let held = h
            .relay
            .prepare_gate
            .acquire_many(PREPARE_GATE)
            .await
            .unwrap();
        let mut sessions = Vec::new();
        for id in 0..8 {
            let (new, _) = h
                .client
                .request(
                    10 + id,
                    "session/new",
                    json!({"cwd": "/", "mcpServers": []}),
                )
                .await;
            let session = new["result"]["sessionId"].as_str().unwrap().to_owned();
            h.client
                .send(prompt(20 + id, &session, &format!("prompt {id}")))
                .await;
            sessions.push(session);
        }
        // Four prompts hold the slots; the other four wait for one.
        tokio::time::timeout(WAIT, async {
            while h.relay.prepared.lock().unwrap().len() < 4 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if !cancel {
            drop(held);
            let mut posted = Vec::new();
            for _ in 0..8 {
                posted.push(recv(&mut h.submitted).await.content);
            }
            posted.sort();
            assert_eq!(
                posted,
                (0..8).map(|id| format!("prompt {id}")).collect::<Vec<_>>()
            );
            continue;
        }
        // Cancel every waiting prompt, one after another.
        let waiting: Vec<u64> = (0..8)
            .filter(|id| {
                !h.relay
                    .prepared
                    .lock()
                    .unwrap()
                    .contains(&format!("prompt {id}"))
            })
            .collect();
        assert_eq!(waiting.len(), 4);
        for id in waiting {
            h.client
                .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":sessions[id as usize]}}))
                .await;
            let (cancelled, _) = h.client.response(20 + id).await;
            assert_eq!(cancelled["result"]["stopReason"], "cancelled");
        }
        drop(held);
        for _ in 0..4 {
            recv(&mut h.submitted).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            h.relay.prepared.lock().unwrap().len(),
            4,
            "a cancelled prompt still prepared"
        );
        assert!(h.submitted.try_recv().is_err());
    }
}

/// A root that the relay definitely refused is forgotten, so the next prompt
/// starts a new thread, even when the prompt was already failed by an
/// unreachable relay. A root whose delivery is unknown may exist, so the
/// next prompt still replies to it.
#[tokio::test]
async fn failed_root_submissions_forget_only_roots_the_relay_refused() {
    for row in ["refused", "unknown", "refused-after-unreachable"] {
        let ids = Ids::new();
        let mut h = start(&ids, Timing::default());
        h.relay_tx.send(RelayInbound::Live).await.unwrap();
        let (new, _) = h
            .client
            .request(1, "session/new", json!({"cwd": "/", "mcpServers": []}))
            .await;
        let session = new["result"]["sessionId"].as_str().unwrap().to_owned();
        *h.relay.submit_failure.lock().unwrap() = Some(match row {
            "unknown" => CliError::DeliveryUnknown("connection closed".into()),
            _ => CliError::Other("rejected".into()),
        });
        let held = h
            .relay
            .submit_gate
            .acquire_many(PREPARE_GATE)
            .await
            .unwrap();
        h.client.send(prompt(2, &session, "first")).await;
        tokio::time::timeout(WAIT, async {
            while h.relay.submits.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if row == "refused-after-unreachable" {
            // The relay drops while the root is being submitted: the prompt
            // fails before the submit result arrives.
            h.relay_tx.send(RelayInbound::Down).await.unwrap();
            h.relay_tx
                .send(RelayInbound::Unreachable("cannot reach the relay".into()))
                .await
                .unwrap();
            let (failed, _) = h.client.response(2).await;
            assert!(failed.get("error").is_some(), "{failed}");
            drop(held);
            tokio::time::sleep(Duration::from_millis(200)).await;
            h.relay_tx.send(RelayInbound::Live).await.unwrap();
        } else {
            drop(held);
            let (failed, _) = h.client.response(2).await;
            assert!(failed.get("error").is_some(), "{failed}");
        }
        h.client.send(prompt(3, &session, "second")).await;
        let second = recv(&mut h.submitted).await;
        let thread = buzz_core::nip10::parse_thread_markers(&second.tags).resolve();
        assert_eq!(thread.is_some(), row == "unknown", "{row}: {:?}", thread);
    }
}

/// A turn bound to telemetry holds agent replies until it ends. Replies past
/// the buffer are not dropped silently: the turn ends with a pointer to the
/// Buzz thread.
#[tokio::test]
async fn replies_beyond_the_turn_buffer_point_to_the_buzz_thread() {
    let ids = Ids::new();
    let channel = ids.channel;
    let mut h = start(&ids, Timing::default());
    h.relay_tx.send(RelayInbound::Live).await.unwrap();
    let (new, _) = h
        .client
        .request(1, "session/new", json!({"cwd": "/", "mcpServers": []}))
        .await;
    let session = new["result"]["sessionId"].as_str().unwrap().to_owned();
    h.client.send(prompt(2, &session, "do a lot")).await;
    let root = recv(&mut h.submitted).await;
    let root_id = root.id.to_hex();
    h.inject(ids.telemetry(&observer(
        "turn_started",
        Some(channel),
        Some("t1"),
        json!({"triggeringEventIds": [root_id]}),
    )))
    .await;
    for index in 0..DEFERRED_REPLY_CAP + 3 {
        h.inject(thread_message(
            &ids.agent,
            channel,
            &root_id,
            &format!("reply {index}"),
        ))
        .await;
    }
    h.inject(ids.telemetry(&observer(
        "turn_completed",
        Some(channel),
        Some("t1"),
        json!({}),
    )))
    .await;
    let (done, notes) = h.client.response(2).await;
    assert_eq!(done["result"]["stopReason"], "end_turn");
    let shown = texts(&notes);
    assert_eq!(shown.len(), DEFERRED_REPLY_CAP + 1, "{shown:?}");
    assert!(
        shown
            .last()
            .unwrap()
            .contains("3 more agent replies from this turn are only in the Buzz thread"),
        "{shown:?}"
    );
}

/// During a long telemetry-bound turn, the agent's thread posts appear after
/// a short delay rather than at the turn's end. A post whose text telemetry
/// streams within that delay is shown once, as streamed text.
#[tokio::test]
async fn agent_thread_posts_appear_during_a_long_turn() {
    let ids = Ids::new();
    let channel = ids.channel;
    let timing = Timing {
        live_reply_delay: Duration::from_millis(200),
        ..Timing::default()
    };
    let mut h = start(&ids, timing);
    h.relay_tx.send(RelayInbound::Live).await.unwrap();
    let (new, _) = h
        .client
        .request(1, "session/new", json!({"cwd": "/", "mcpServers": []}))
        .await;
    let session = new["result"]["sessionId"].as_str().unwrap().to_owned();
    h.client.send(prompt(2, &session, "review the PRs")).await;
    let root = recv(&mut h.submitted).await;
    let root_id = root.id.to_hex();
    h.inject(ids.telemetry(&observer(
        "turn_started",
        Some(channel),
        Some("t1"),
        json!({"triggeringEventIds": [root_id]}),
    )))
    .await;
    // A post telemetry never streams, and one it streams within the delay.
    h.inject(thread_message(
        &ids.agent,
        channel,
        &root_id,
        "Merged #440 and #2337.",
    ))
    .await;
    h.inject(thread_message(
        &ids.agent,
        channel,
        &root_id,
        "Narrated and posted",
    ))
    .await;
    h.inject(ids.telemetry(&observer(
        "acp_read",
        Some(channel),
        Some("t1"),
        chunk("Narrated and posted"),
    )))
    .await;
    // Before the turn ends, the client sees the first post exactly once.
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !seen
        .iter()
        .any(|text: &String| text.contains("Merged #440"))
    {
        let line = tokio::time::timeout_at(deadline, h.client.stdout.next_line())
            .await
            .expect("the post appeared before the turn ended")
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert!(frame.get("id").is_none(), "the turn ended first: {frame}");
        seen.extend(texts(&[frame]));
    }
    // Let every held reply's release run before the turn ends, so the
    // release-time streamed-text check, not the turn-end flush, decides the
    // second post. Anything released meanwhile is read with the response.
    tokio::time::sleep(Duration::from_millis(600)).await;
    h.inject(ids.telemetry(&observer(
        "turn_completed",
        Some(channel),
        Some("t1"),
        json!({}),
    )))
    .await;
    let (done, notes) = h.client.response(2).await;
    assert_eq!(done["result"]["stopReason"], "end_turn");
    seen.extend(texts(&notes));
    let count = |needle: &str| seen.iter().filter(|text| text.contains(needle)).count();
    assert_eq!(count("Merged #440"), 1, "{seen:?}");
    assert_eq!(count("Narrated and posted"), 1, "{seen:?}");
}

/// An unreachable relay fails the running prompt and every new prompt
/// visibly, keeps the bridge running, and clears once the relay is live.
#[tokio::test]
async fn unreachable_relay_fails_prompts_until_it_is_live_again() {
    let ids = Ids::new();
    let mut h = start(&ids, Timing::default());
    let (new, _) = h
        .client
        .request(1, "session/new", json!({"cwd": "/", "mcpServers": []}))
        .await;
    let session = new["result"]["sessionId"].as_str().unwrap().to_owned();
    h.client.send(prompt(2, &session, "hello")).await;
    // Client frames are handled in order: once this answers, the prompt is pending.
    h.client
        .request(3, "initialize", json!({"protocolVersion": 1}))
        .await;
    h.relay_tx.send(RelayInbound::Down).await.unwrap();
    h.relay_tx
        .send(RelayInbound::Unreachable("cannot reach the relay".into()))
        .await
        .unwrap();
    let (failed, _) = h.client.response(2).await;
    assert_eq!(failed["error"]["message"], "cannot reach the relay");

    // While unreachable, a new prompt fails at once instead of waiting.
    let (refused, _) = h
        .client
        .request(
            4,
            "session/prompt",
            prompt(4, &session, "again")["params"].clone(),
        )
        .await;
    assert_eq!(refused["error"]["message"], "cannot reach the relay");
    assert!(!h.bridge.is_finished(), "the bridge keeps running");

    // Live again: the next prompt posts.
    h.relay_tx.send(RelayInbound::Live).await.unwrap();
    h.client.send(prompt(5, &session, "back")).await;
    let posted = recv(&mut h.submitted).await;
    assert_eq!(posted.content, "back");
}

async fn new_session(h: &mut Harness, id: u64) -> Value {
    h.client
        .request(id, "session/new", json!({"cwd": "/", "mcpServers": []}))
        .await
        .0
}

/// At the session cap, `session/new` evicts the least recently used idle
/// session, and refuses only when every session has a running prompt.
#[tokio::test]
async fn session_cap_evicts_the_least_recently_used_idle_session() {
    use super::bridge::MAX_SESSIONS;

    let ids = Ids::new();
    let mut h = start(&ids, Timing::default());
    let mut sessions = Vec::new();
    for n in 0..MAX_SESSIONS as u64 {
        let (new, _) = h
            .client
            .request(n + 1, "session/new", json!({"cwd": "/", "mcpServers": []}))
            .await;
        sessions.push(new["result"]["sessionId"].as_str().unwrap().to_owned());
    }
    let mut id = 1000;
    let mut next_id = || {
        id += 1;
        id
    };
    // The relay is never live, so prompts stay running. Session 0 is the
    // oldest but busy; session 1 is the least recently used idle one.
    h.client.send(prompt(next_id(), &sessions[0], "busy")).await;
    let created = new_session(&mut h, next_id()).await;
    assert!(created["result"]["sessionId"].is_string(), "{created}");
    let (evicted, _) = h
        .client
        .request(
            next_id(),
            "session/prompt",
            prompt(0, &sessions[1], "hi")["params"].clone(),
        )
        .await;
    assert_eq!(
        evicted["error"]["message"],
        format!("unknown session: {}", sessions[1])
    );

    // Make every session busy: the next `session/new` is refused.
    let mut busy = vec![created["result"]["sessionId"].as_str().unwrap().to_owned()];
    busy.extend(sessions[2..].iter().cloned());
    for session in &busy {
        h.client.send(prompt(next_id(), session, "busy")).await;
    }
    let refused = new_session(&mut h, next_id()).await;
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("running prompts")),
        "{refused}"
    );
}

/// Backoff doubles from 1 s, never exceeds the 60 s cap, and jitter keeps
/// each wait between half and all of the exponential step.
#[test]
fn backoff_is_exponential_jittered_and_capped() {
    use super::backoff_delay;

    let secs = |s: u64| Duration::from_secs(s);
    for (failures, full) in [
        (1, 1),
        (2, 2),
        (3, 4),
        (6, 32),
        (7, 60),
        (40, 60),
        (u32::MAX, 60),
    ] {
        assert_eq!(
            backoff_delay(failures, 1.0),
            secs(full),
            "failures={failures}"
        );
        assert_eq!(
            backoff_delay(failures, 0.0),
            secs(full) / 2,
            "failures={failures}"
        );
        let jittered = backoff_delay(failures, 0.37);
        assert!(jittered > secs(full) / 2 && jittered < secs(full));
    }
}

/// A fake connection for [`super::supervise`]: records when each attempt
/// starts, optionally reports live, stays up for `up`, then fails.
fn flapping(
    starts: Arc<std::sync::Mutex<Vec<tokio::time::Instant>>>,
    behaviour: impl Fn(usize) -> (Duration, bool) + Send + 'static,
) -> impl FnMut(
    u64,
    mpsc::Sender<RelayInbound>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = super::Stream> + Send>> {
    move |_since, tx| {
        let attempt = {
            let mut starts = starts.lock().unwrap();
            starts.push(tokio::time::Instant::now());
            starts.len() - 1
        };
        let (up, live_event) = behaviour(attempt);
        Box::pin(async move {
            if tx.send(RelayInbound::Live).await.is_err() {
                return super::Stream::ReceiverGone;
            }
            let live_at = tokio::time::Instant::now();
            tokio::time::sleep(up).await;
            super::Stream::Failed(super::Failure {
                error: "dropped".into(),
                live_at: Some(live_at),
                live_event,
            })
        })
    }
}

/// Gaps in whole seconds between consecutive connection attempts.
fn gaps(starts: &[tokio::time::Instant], up: impl Fn(usize) -> Duration) -> Vec<u64> {
    starts
        .windows(2)
        .enumerate()
        .map(|(i, w)| (w[1] - w[0] - up(i)).as_secs())
        .collect()
}

/// Drain the subscription channel until `attempts` connections started,
/// returning the non-event messages, then stop the supervisor.
async fn drive(
    attempts: usize,
    starts: &Arc<std::sync::Mutex<Vec<tokio::time::Instant>>>,
    mut rx: mpsc::Receiver<RelayInbound>,
    task: tokio::task::JoinHandle<()>,
) -> Vec<String> {
    let mut seen = Vec::new();
    while starts.lock().unwrap().len() < attempts {
        match rx.recv().await.expect("supervisor running") {
            RelayInbound::Live => seen.push("live".to_owned()),
            RelayInbound::Down => seen.push("down".to_owned()),
            RelayInbound::Unreachable(m) => seen.push(format!("unreachable: {m}")),
            RelayInbound::Event(_) => {}
        }
    }
    drop(rx);
    task.abort();
    seen
}

/// A relay that accepts each connection and drops it at once is backed off
/// exponentially to the cap, reported unreachable, and still retried.
#[tokio::test(start_paused = true)]
async fn relay_that_drops_every_connection_backs_off_and_is_surfaced() {
    let starts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(super::supervise(
        tx,
        || 1.0,
        flapping(starts.clone(), |_| (Duration::ZERO, false)),
    ));
    let seen = drive(10, &starts, rx, task).await;
    let starts = starts.lock().unwrap().clone();
    assert_eq!(
        gaps(&starts, |_| Duration::ZERO),
        [1, 2, 4, 8, 16, 32, 60, 60, 60]
    );
    let unreachable: Vec<_> = seen
        .iter()
        .filter(|m| m.starts_with("unreachable"))
        .collect();
    assert_eq!(
        unreachable.len(),
        9 - 4,
        "failures 5..=9 are surfaced: {seen:?}"
    );
    assert!(unreachable[0].contains("5 failed connections in a row: dropped"));
    assert!(unreachable[0].contains("still retrying"));
}

/// A connection that stays live for the healthy period, or delivers a live
/// event, resets the backoff; a shorter, silent one does not.
#[tokio::test(start_paused = true)]
async fn healthy_connection_resets_the_backoff() {
    // Attempts: 0-2 drop at once, 3 stays 30 s, 4-5 drop, 6 delivers an
    // event after 1 s, 7-8 drop, 9 stays 29 s.
    let up = |attempt: usize| match attempt {
        3 => Duration::from_secs(30),
        6 => Duration::from_secs(1),
        9 => Duration::from_secs(29),
        _ => Duration::ZERO,
    };
    let starts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(super::supervise(
        tx,
        || 1.0,
        flapping(starts.clone(), move |attempt| (up(attempt), attempt == 6)),
    ));
    drive(11, &starts, rx, task).await;
    let starts = starts.lock().unwrap().clone();
    assert_eq!(gaps(&starts, up), [1, 2, 4, 1, 2, 4, 1, 2, 4, 8]);
}

#[test]
fn telemetry_is_accepted_only_from_the_agent_to_the_signer() {
    let ids = Ids::new();
    let scope = ids.scope();
    let plaintext = observer("turn_completed", None, Some("t"), json!({}));

    let accepted = decode_telemetry(&scope, &ids.telemetry(&plaintext)).unwrap();
    assert_eq!(accepted.len(), 1, "frames without a channelId are kept");

    let someone_else = Keys::generate();
    let not_for_us = telemetry_from(
        &ids.agent,
        &ids.agent.public_key(),
        &someone_else.public_key(),
        &plaintext,
    );
    assert_eq!(
        decode_telemetry(&scope, &not_for_us).unwrap_err(),
        FrameError::NotForSigner
    );

    let control = cancel_turn_frame(&scope, "r1").unwrap();
    assert_eq!(
        decode_telemetry(&scope, &control).unwrap_err(),
        FrameError::NotTelemetry
    );

    let other_agent = Keys::generate();
    let from_other_agent = telemetry_from(
        &other_agent,
        &other_agent.public_key(),
        &ids.signer.public_key(),
        &plaintext,
    );
    assert_eq!(
        decode_telemetry(&scope, &from_other_agent).unwrap_err(),
        FrameError::WrongAgent
    );

    // Valid tags, but content not decryptable by the signer.
    let garbled = EventBuilder::new(Kind::Custom(24200), "A".repeat(200))
        .tags(ids.telemetry(&plaintext).tags.iter().cloned())
        .sign_with_keys(&ids.agent)
        .unwrap();
    assert!(matches!(
        decode_telemetry(&scope, &garbled).unwrap_err(),
        FrameError::Decrypt(_)
    ));
}

#[test]
fn observer_events_map_to_bridge_signals() {
    let event = |kind: &str, turn: Option<&str>, payload: Value| {
        signal_of(serde_json::from_value(observer(kind, None, turn, payload)).unwrap())
    };
    let cases = [
        (
            event(
                "turn_started",
                Some("t"),
                json!({"triggeringEventIds": ["a", 1, "b"]}),
            ),
            Signal::TurnStarted {
                turn_id: "t".into(),
                triggering: vec!["a".into(), "b".into()],
            },
        ),
        (
            event("turn_completed", Some("t"), json!({})),
            Signal::TurnCompleted {
                turn_id: "t".into(),
            },
        ),
        (
            event("turn_liveness", Some("t"), json!({})),
            Signal::Liveness {
                turn_id: "t".into(),
            },
        ),
        (
            event("acp_read", Some("t"), chunk("x")),
            Signal::Update {
                turn_id: "t".into(),
                update: json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "x"}}),
            },
        ),
        (
            event(
                "acp_read",
                Some("t"),
                acp_update(json!({"sessionUpdate": "user_message_chunk"})),
            ),
            Signal::Ignored,
        ),
        (
            event(
                "acp_read",
                Some("t"),
                json!({"id": 3, "result": {"stopReason": "refusal"}}),
            ),
            Signal::StopReason {
                turn_id: "t".into(),
                reason: "refusal".into(),
                usage: None,
            },
        ),
        (
            event(
                "acp_read",
                Some("t"),
                json!({"id": 3, "result": {"stopReason": "end_turn", "usage": {
                    "inputTokens": 1, "outputTokens": 2, "totalTokens": 3, "thoughtTokens": 4,
                    "cachedReadTokens": 5, "cachedWriteTokens": 6,
                }}}),
            ),
            Signal::StopReason {
                turn_id: "t".into(),
                reason: "end_turn".into(),
                usage: Some(json!({
                    "inputTokens": 1, "outputTokens": 2, "totalTokens": 3, "thoughtTokens": 4,
                    "cachedReadTokens": 5, "cachedWriteTokens": 6,
                })),
            },
        ),
        (
            // An optional count that is not a non-negative integer is dropped.
            event(
                "acp_read",
                Some("t"),
                json!({"id": 3, "result": {"stopReason": "end_turn", "usage": {
                    "inputTokens": 1, "outputTokens": 2, "totalTokens": 3,
                    "thoughtTokens": 1.5, "cachedReadTokens": "5", "cachedWriteTokens": -6,
                }}}),
            ),
            Signal::StopReason {
                turn_id: "t".into(),
                reason: "end_turn".into(),
                usage: Some(json!({"inputTokens": 1, "outputTokens": 2, "totalTokens": 3})),
            },
        ),
        (
            // A missing or invalid required count drops the whole usage.
            event(
                "acp_read",
                Some("t"),
                json!({"id": 3, "result": {"stopReason": "end_turn", "usage": {
                    "inputTokens": 1, "outputTokens": -2, "totalTokens": 3,
                }}}),
            ),
            Signal::StopReason {
                turn_id: "t".into(),
                reason: "end_turn".into(),
                usage: None,
            },
        ),
        (
            event(
                "acp_read",
                Some("t"),
                json!({"id": 3, "result": {"stopReason": "end_turn", "usage": {
                    "inputTokens": 1, "totalTokens": 3,
                }}}),
            ),
            Signal::StopReason {
                turn_id: "t".into(),
                reason: "end_turn".into(),
                usage: None,
            },
        ),
        (
            event(
                "acp_read",
                Some("t"),
                json!({"id": 3, "result": {"stopReason": "end_turn", "usage": "lots"}}),
            ),
            Signal::StopReason {
                turn_id: "t".into(),
                reason: "end_turn".into(),
                usage: None,
            },
        ),
        (
            event(
                "acp_read",
                Some("t"),
                acp_update(json!({"sessionUpdate": "usage_update", "size": 10, "used": 0})),
            ),
            Signal::Update {
                turn_id: "t".into(),
                update: json!({"sessionUpdate": "usage_update", "size": 10, "used": 0}),
            },
        ),
        (
            // A cost without a usable amount or currency is dropped.
            event(
                "acp_read",
                Some("t"),
                acp_update(json!({
                    "sessionUpdate": "usage_update", "size": 10, "used": 4,
                    "cost": {"amount": "1", "currency": "USD"},
                })),
            ),
            Signal::Update {
                turn_id: "t".into(),
                update: json!({"sessionUpdate": "usage_update", "size": 10, "used": 4}),
            },
        ),
        (
            event(
                "acp_read",
                Some("t"),
                acp_update(json!({
                    "sessionUpdate": "usage_update", "size": 10, "used": 4,
                    "cost": {"amount": 1, "currency": "X".repeat(17)},
                })),
            ),
            Signal::Update {
                turn_id: "t".into(),
                update: json!({"sessionUpdate": "usage_update", "size": 10, "used": 4}),
            },
        ),
        (
            event(
                "acp_read",
                Some("t"),
                acp_update(json!({
                    "sessionUpdate": "usage_update", "size": 10, "used": 4, "cost": {"amount": -0.5},
                })),
            ),
            Signal::Update {
                turn_id: "t".into(),
                update: json!({"sessionUpdate": "usage_update", "size": 10, "used": 4}),
            },
        ),
    ];
    for (index, (got, want)) in cases.into_iter().enumerate() {
        assert_eq!(got, want, "case {index}");
    }
    // A usage_update whose size or used is not a usable token count is dropped.
    for (size, used) in [
        (json!(0), json!(1)),
        (json!(-1), json!(1)),
        (json!(1.5), json!(1)),
        (json!("10"), json!(1)),
        (Value::Null, json!(1)),
        (json!(10), json!(-1)),
        (json!(10), json!(0.5)),
        (json!(10), json!("1")),
        (json!(10), Value::Null),
    ] {
        let update =
            acp_update(json!({"sessionUpdate": "usage_update", "size": size, "used": used}));
        assert_eq!(
            event("acp_read", Some("t"), update),
            Signal::Ignored,
            "size {size}, used {used}"
        );
    }
    let cases = [
        (
            event(
                "acp_read",
                Some("t"),
                json!({"id": 3, "result": {"stopReason": "bogus"}}),
            ),
            Signal::Ignored,
        ),
        (event("acp_read", None, chunk("no turn")), Signal::Ignored),
        (
            event(
                "control_result",
                None,
                json!({"type": "cancel_turn", "status": "no_active_turn", "requestId": "r"}),
            ),
            Signal::CancelResult {
                request_id: "r".into(),
                status: "no_active_turn".into(),
            },
        ),
        (
            event(
                "control_result",
                None,
                json!({"type": "switch_model", "status": "sent", "requestId": "r"}),
            ),
            Signal::Ignored,
        ),
    ];
    for (index, (got, want)) in cases.into_iter().enumerate() {
        assert_eq!(got, want, "case {index}");
    }
}

#[test]
fn relay_url_must_be_wss() {
    assert!(require_wss("wss://relay.example").is_ok());
    assert!(require_wss("wss://relay.example:8443/").is_ok());
    for refused in [
        "ws://relay.example",
        "https://relay.example",
        "http://localhost:3000",
        "relay.example",
        "",
    ] {
        assert!(require_wss(refused).is_err(), "{refused}");
    }
}

/// The refusal is wired into command dispatch, before any key or relay use.
#[tokio::test]
async fn acp_bridge_command_refuses_plain_ws() {
    let agent = Keys::generate().public_key().to_hex();
    let channel = Uuid::new_v4().to_string();
    let code = crate::run_from_args([
        "buzz",
        "--relay",
        "ws://relay.example",
        "--private-key",
        &Keys::generate().secret_key().to_secret_hex(),
        "acp-bridge",
        "--agent",
        &agent,
        "--channel",
        &channel,
    ])
    .await;
    assert_eq!(code, 1);
}

#[test]
fn prompt_content_blocks_become_message_text() {
    let text = super::prompt::parse(Some(&json!([
        {"type": "text", "text": "Fix the build"},
        {"type": "resource_link", "uri": "file:///repo/build.log", "name": "build.log"},
    ])))
    .unwrap();
    assert_eq!(
        text.text,
        "Fix the build\n\n[build.log](file:///repo/build.log)"
    );
    assert!(super::prompt::parse(Some(
        &json!([{"type": "image", "data": "", "mimeType": "image/png"}])
    ))
    .is_err());
    assert!(super::prompt::parse(Some(&json!([{"type": "text", "text": "  "}]))).is_err());
    assert!(super::prompt::parse(None).is_err());
}

#[test]
fn recovery_replies_are_parsed_exactly() {
    use super::bridge::{parse_recovery, Recovery};
    let cases = [
        ("/recover", Some((Recovery::SameThread, ""))),
        ("  /fresh  ", Some((Recovery::NewThread, ""))),
        (
            "/fresh try again\nwith context",
            Some((Recovery::NewThread, "try again\nwith context")),
        ),
        ("/recovery", None),
        ("please /recover", None),
        ("/Fresh", None),
    ];
    for (text, want) in cases {
        assert_eq!(parse_recovery(text), want, "{text:?}");
    }
}

#[path = "history_tests.rs"]
mod history_tests;

#[path = "history_regressions.rs"]
mod history_regressions;

#[path = "media_tests.rs"]
mod media_tests;

#[path = "participant_tests.rs"]
mod participant_tests;

#[path = "participation_tests.rs"]
mod participation_tests;

#[path = "transcript_replay_tests.rs"]
mod transcript_replay_tests;
