//! The ACP agent loop: maps ACP sessions to Buzz threads and remote agent
//! turns to ACP prompt turns.
//!
//! One task owns all state. It reads client frames, relay events, and the
//! results of background relay calls from bounded channels, so the relay
//! boundary can be replaced in tests (see [`BridgeRelay`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use nostr::{Event, EventId};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::Instant;

use super::frames::{self, Scope, Signal};
use super::names::{self, Names};
use super::projects;
use super::prompt::{self, Prompt};
use super::wire::{self, Inbound, StdinMsg};
use crate::error::CliError;

#[path = "history.rs"]
mod history;

/// ACP protocol version the bridge speaks.
const PROTOCOL_VERSION: u64 = 1;
/// Most ACP sessions (one Buzz thread each) a bridge process keeps. At the
/// cap, `session/new` evicts the least recently used session without a
/// running prompt.
pub(crate) const MAX_SESSIONS: usize = 64;
/// Relay event ids remembered for de-duplication across reconnect replays.
const SEEN_EVENT_CAP: usize = 4096;
const HISTORY_LIVE_BYTES: usize = 4 * 1024 * 1024;
/// Own prompt ids remembered per session, to skip their relay echo.
const OWN_EVENT_CAP: usize = 256;
/// Agent thread replies held per session while telemetry may still stream
/// the same text.
pub(crate) const DEFERRED_REPLY_CAP: usize = 32;
/// Most streamed agent text kept per prompt for reply de-duplication. Text
/// past the cap is not compared, so a reply repeating it is shown twice
/// rather than lost.
const STREAMED_TEXT_CAP: usize = 256 * 1024;
/// Wait before re-signing a prompt identical to an earlier one, so its
/// one-second timestamp, and with it the event id, differs.
const DUPLICATE_RESIGN_DELAY: Duration = Duration::from_millis(1100);
const OUT_QUEUE: usize = 256;
const IN_QUEUE: usize = 4;
const OPS_QUEUE: usize = 64;
/// Time allowed for queued client frames to drain after the loop ends.
const WRITER_DRAIN: Duration = Duration::from_secs(5);

/// Start of the reply the sandbox manager posts when a mention targets an
/// ended thread session (`failure_notice` in
/// `crates/buzz-sandbox-reaper/src/mentions/worker.rs`).
pub(crate) const SESSION_ENDED_NOTICE: &str = "This thread's agent session ended";
/// Most characters of the manager's ended notice repeated to the user.
const ENDED_REASON_CHARS: usize = 1000;

/// Reply that resends the unanswered message in the same thread.
pub(crate) const RECOVER_COMMAND: &str = "/recover";
/// Reply that resends the unanswered message as a new thread.
pub(crate) const FRESH_COMMAND: &str = "/fresh";

/// The relay operations the bridge needs. Production uses the buzz-cli
/// client; tests use an in-memory fake.
pub(crate) trait BridgeRelay: Send + Sync + 'static {
    /// Query signed stored events; failures must propagate.
    fn query(&self, _filter: Value) -> impl Future<Output = Result<Vec<Event>, CliError>> + Send {
        async { Err(CliError::Other("history queries unavailable".into())) }
    }

    /// Build and sign a kind 9 prompt that mentions the agent: a thread root
    /// when `thread_root` is `None`, otherwise a reply in that thread.
    fn prepare_prompt(
        &self,
        content: String,
        thread_root: Option<String>,
    ) -> impl Future<Output = Result<Event, CliError>> + Send;

    /// Prepare bounded image content; relays without upload support refuse it.
    fn prepare_content(
        &self,
        prompt: Prompt,
        thread_root: Option<String>,
    ) -> impl Future<Output = Result<Event, CliError>> + Send {
        async move {
            if !prompt.images.is_empty() || !prompt.media.is_empty() {
                return Err(CliError::Other("image upload unavailable".into()));
            }
            self.prepare_prompt(prompt.text, thread_root).await
        }
    }

    /// Store a signed event on the relay.
    fn submit(&self, event: Event) -> impl Future<Output = Result<(), CliError>> + Send;

    /// Publish a signed ephemeral event (an observer control frame).
    fn publish_ephemeral(&self, event: Event) -> impl Future<Output = Result<(), CliError>> + Send;
}

/// Events from the relay subscription task.
#[derive(Debug)]
pub(crate) enum RelayInbound {
    /// Subscriptions are installed and caught up; prompts may be posted.
    Live,
    /// The connection dropped; a reconnect is in progress.
    Down,
    /// A telemetry frame or a channel message.
    Event(Box<Event>),
    /// Reconnects keep failing. Running prompts fail with this message, and
    /// new ones fail until the subscription is live again. Reconnects
    /// continue.
    Unreachable(String),
}

/// Bridge timeouts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// Without telemetry, how long after an agent reply in the thread the
    /// prompt ends. Telemetry can be absent when the signer is not the
    /// agent's owner, because frames are encrypted to the owner.
    pub no_telemetry_grace: Duration,
    /// How long a cancelled prompt waits for the turn to end before it
    /// reports `cancelled` anyway.
    pub cancel_wait: Duration,
    /// After a turn has sent liveness frames, how long a silent turn may last
    /// before the prompt fails (for example, frames lost in a reconnect).
    pub turn_silence: Duration,
    /// How long an agent reply that arrives during a telemetry-bound turn is
    /// held before it is shown: long enough for trailing telemetry of the
    /// same text to arrive, so the reply is not shown twice, and short enough
    /// that a long turn still shows the agent's thread posts as they happen.
    pub live_reply_delay: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            no_telemetry_grace: Duration::from_secs(15),
            cancel_wait: Duration::from_secs(15),
            turn_silence: Duration::from_secs(120),
            live_reply_delay: Duration::from_secs(3),
        }
    }
}

/// A bounded set of recently seen event ids.
struct RecentIds {
    order: VecDeque<EventId>,
    set: HashSet<EventId>,
    cap: usize,
}

impl RecentIds {
    fn new(cap: usize) -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
            cap,
        }
    }

    /// Insert `id`; returns false when it was already present.
    fn insert(&mut self, id: EventId) -> bool {
        if !self.set.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }

    fn contains(&self, id: &EventId) -> bool {
        self.set.contains(id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the relay subscription before posting.
    AwaitingRelay,
    Preparing,
    Submitting,
    Posted,
}

struct CancelState {
    request_id: String,
    deadline: Instant,
}

struct PendingPrompt {
    request_id: Value,
    generation: u64,
    prompt: Prompt,
    phase: Phase,
    /// The posted prompt's event id, known once it is signed.
    event_id: Option<String>,
    /// The posted prompt's timestamp, known once it is signed.
    created_at: Option<nostr::Timestamp>,
    /// Whether this prompt created the thread root.
    is_root: bool,
    /// The remote turn whose triggering events include this prompt.
    turn_id: Option<String>,
    stop_reason: Option<String>,
    /// The remote prompt response's validated ACP `Usage`, if it reported one.
    usage: Option<Value>,
    cancel: Option<CancelState>,
    fallback_deadline: Option<Instant>,
    silence_deadline: Option<Instant>,
    /// The background preparation, stopped when this prompt is dropped.
    prepare: Option<PrepareTask>,
}

/// Aborts a prompt's preparation when the prompt is cancelled, finished, or
/// replaced, so a prompt that is gone never waits for a slot, uploads, or signs.
struct PrepareTask(tokio::task::AbortHandle);

impl Drop for PrepareTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Session {
    id: String,
    /// Changes on replay installation, fencing pending presentation lookups.
    replay_epoch: u64,
    /// Thread root event id, once the first prompt is signed.
    root: Option<String>,
    /// The `<owner>/<repo>` project of the `session/new` working directory,
    /// or of a loaded thread's root; recorded on each new thread root.
    project: Option<String>,
    /// The sandbox manager's notice that this thread's agent session ended,
    /// up to [`ENDED_REASON_CHARS`]. New prompts are not posted until the
    /// user replies `/recover` or `/fresh`.
    ended: Option<String>,
    generation: u64,
    /// [`Bridge::tick`] when the session was created or last prompted.
    last_used: u64,
    own_events: RecentIds,
    /// Agent message text telemetry streamed since the latest prompt, up to
    /// [`STREAMED_TEXT_CAP`]. An agent kind 9 reply contained in it repeats
    /// streamed text and is not emitted. Agents post replies with
    /// `buzz messages send`, so a reply usually differs from the streamed
    /// narration and is shown.
    streamed: String,
    /// The agent's kind 9 replies that arrived during a telemetry-bound turn,
    /// each with the time it may be shown. Each is emitted once that time
    /// passes or the turn ends, except those the turn streamed.
    deferred: Vec<(String, Instant)>,
    /// Agent replies not held because [`DEFERRED_REPLY_CAP`] was reached.
    deferred_dropped: usize,
    /// The user's text of the latest posted prompt, resent by `/recover` and
    /// `/fresh`.
    last_prompt: Option<Prompt>,
    pending: Option<PendingPrompt>,
}

/// A reply that continues an ended session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// Resend in the same thread. The new mention lets the sandbox manager
    /// try to restore the session.
    SameThread,
    /// Resend as a new thread root: a new session with an empty workspace.
    NewThread,
}

/// Parse a `/recover` or `/fresh` reply. Returns the command and the text
/// after it, which replaces the resent message when not empty.
pub(crate) fn parse_recovery(text: &str) -> Option<(Recovery, &str)> {
    let text = text.trim();
    let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let command = match word {
        RECOVER_COMMAND => Recovery::SameThread,
        FRESH_COMMAND => Recovery::NewThread,
        _ => return None,
    };
    Some((command, rest.trim()))
}

/// Results of background relay calls, fenced by session generation.
enum Op {
    PeerNameWarmed,
    HistoryListed {
        id: Value,
        result: Result<Value, CliError>,
    },
    HistoryLoaded {
        id: Value,
        session: String,
        token: u64,
        prior: Option<(u64, u64)>,
        result: Result<history::Replay, CliError>,
    },
    Prepared {
        session: String,
        generation: u64,
        result: Result<Event, CliError>,
    },
    Submitted {
        session: String,
        generation: u64,
        /// The submitted event's id when it created the thread root.
        root: Option<String>,
        result: Result<(), CliError>,
    },
    CancelPublished {
        session: String,
        generation: u64,
        result: Result<(), CliError>,
    },
}

struct HistoryLoad {
    token: u64,
    overflowed: bool,
    /// The project of the client's `cwd`, adopted by an untagged root.
    project: Option<String>,
}

/// Run the bridge until the client closes stdin (`Ok`) or stdout, stdin, or
/// the relay subscription fails (`Err`).
pub(crate) async fn run_bridge<R, I, O>(
    scope: Scope,
    relay: Arc<R>,
    mut relay_rx: mpsc::Receiver<RelayInbound>,
    input: I,
    output: O,
    timing: Timing,
) -> Result<(), CliError>
where
    R: BridgeRelay,
    I: AsyncBufRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, out_rx) = mpsc::channel(OUT_QUEUE);
    let writer = tokio::spawn(wire::write_frames(out_rx, output));
    let (in_tx, mut in_rx) = mpsc::channel(IN_QUEUE);
    let reader = tokio::spawn(wire::read_frames(input, in_tx));
    let (ops_tx, mut ops_rx) = mpsc::channel(OPS_QUEUE);

    let mut bridge = Bridge {
        scope,
        relay,
        out: out_tx,
        ops: ops_tx,
        timing,
        sessions: HashMap::new(),
        history_ops: 0,
        loading: HashMap::new(),
        history_live: Vec::new(),
        history_live_bytes: 0,
        relay_live: false,
        relay_error: None,
        seen: RecentIds::new(SEEN_EVENT_CAP),
        tick: 0,
        names: Arc::new(Names::default()),
        peer_names: 0,
        prepares: Arc::new(Semaphore::new(4)),
    };
    let result = bridge.run(&mut in_rx, &mut relay_rx, &mut ops_rx).await;
    reader.abort();
    drop(bridge);
    if tokio::time::timeout(WRITER_DRAIN, writer).await.is_err() {
        eprintln!("acp-bridge: stdout did not drain before exit");
    }
    result
}

struct Bridge<R> {
    names: Arc<Names>,
    peer_names: usize,
    prepares: Arc<Semaphore>,
    scope: Scope,
    relay: Arc<R>,
    out: mpsc::Sender<Value>,
    ops: mpsc::Sender<Op>,
    timing: Timing,
    sessions: HashMap<String, Session>,
    history_ops: usize,
    loading: HashMap<String, HistoryLoad>,
    history_live: Vec<Event>,
    history_live_bytes: usize,
    relay_live: bool,
    /// Set while reconnects keep failing; cleared when the relay is live.
    relay_error: Option<String>,
    seen: RecentIds,
    /// Counter that orders session use, for eviction.
    tick: u64,
}

fn stdout_closed() -> CliError {
    CliError::Other("acp-bridge: stdout closed".into())
}

impl<R: BridgeRelay> Bridge<R> {
    async fn run(
        &mut self,
        in_rx: &mut mpsc::Receiver<StdinMsg>,
        relay_rx: &mut mpsc::Receiver<RelayInbound>,
        ops_rx: &mut mpsc::Receiver<Op>,
    ) -> Result<(), CliError> {
        loop {
            let deadline = self.next_deadline();
            let sleep_until = deadline.unwrap_or_else(Instant::now);
            tokio::select! {
                msg = in_rx.recv() => match msg {
                    Some(StdinMsg::Frame(frame)) => self.on_client_frame(frame).await?,
                    Some(StdinMsg::Failed(e)) => {
                        return Err(CliError::Other(format!("acp-bridge: stdin read failed: {e}")));
                    }
                    Some(StdinMsg::Eof) | None => return Ok(()),
                },
                inbound = relay_rx.recv() => match inbound {
                    Some(RelayInbound::Live) => {
                        self.relay_live = true;
                        self.relay_error = None;
                        self.start_waiting_prompts();
                    }
                    Some(RelayInbound::Down) => self.relay_live = false,
                    Some(RelayInbound::Event(event)) => self.on_relay_event(*event).await?,
                    Some(RelayInbound::Unreachable(message)) => {
                        eprintln!("{message}");
                        self.fail_all_prompts(&message).await?;
                        self.relay_error = Some(message);
                    }
                    None => {
                        let message = "acp-bridge: relay subscription ended".to_string();
                        self.fail_all_prompts(&message).await?;
                        return Err(CliError::Other(message));
                    }
                },
                Some(op) = ops_rx.recv() => self.on_op(op).await?,
                _ = tokio::time::sleep_until(sleep_until), if deadline.is_some() => {
                    self.on_deadlines().await?;
                }
            }
        }
    }

    async fn send(&self, frame: Value) -> Result<(), CliError> {
        self.out.send(frame).await.map_err(|_| stdout_closed())
    }

    async fn respond_error(&self, id: Value, code: i32, message: &str) -> Result<(), CliError> {
        self.send(wire::err(id, code, message)).await
    }

    // ---- client frames -------------------------------------------------

    async fn on_client_frame(&mut self, frame: Inbound) -> Result<(), CliError> {
        match frame {
            Inbound::Request { id, method, params } => self.on_request(id, &method, params).await,
            Inbound::Notification { method, params } => {
                if method == "session/cancel" {
                    self.on_cancel(&params).await?;
                }
                Ok(())
            }
            Inbound::Response => Ok(()),
            Inbound::Invalid { id, code, message } => self.respond_error(id, code, &message).await,
        }
    }

    async fn on_request(&mut self, id: Value, method: &str, params: Value) -> Result<(), CliError> {
        match method {
            "initialize" => {
                self.send(wire::ok(
                    id,
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "agentCapabilities": {
                            "loadSession": true,
                            "sessionCapabilities": {"list": {}},
                            "promptCapabilities": {
                                "image": true,
                                "audio": false,
                                "embeddedContext": false,
                            },
                        },
                        "authMethods": [],
                        "agentInfo": {
                            "name": "buzz-acp-bridge",
                            "title": "Buzz remote agent",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    }),
                ))
                .await
            }
            "authenticate" => self.send(wire::ok(id, json!({}))).await,
            "session/new" => self.on_session_new(id, &params).await,
            "session/list" => self.on_history_list(id, &params).await,
            "session/load" => self.on_history_load(id, &params).await,
            "session/prompt" => self.on_prompt(id, &params).await,
            _ => {
                self.respond_error(
                    id,
                    wire::METHOD_NOT_FOUND,
                    &format!("method not supported by buzz acp-bridge: {method}"),
                )
                .await
            }
        }
    }

    async fn on_session_new(&mut self, id: Value, params: &Value) -> Result<(), CliError> {
        if self.sessions.len() >= MAX_SESSIONS && !self.evict_idle_session() {
            return self
                .respond_error(
                    id,
                    wire::INTERNAL_ERROR,
                    &format!(
                        "acp-bridge has {MAX_SESSIONS} sessions with running prompts; wait for one to finish"
                    ),
                )
                .await;
        }
        let session_id = uuid::Uuid::new_v4().to_string();
        let last_used = self.next_tick();
        let project = params
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(|cwd| projects::project_of_cwd(&self.scope.home, cwd));
        self.sessions.insert(
            session_id.clone(),
            Session {
                id: session_id.clone(),
                replay_epoch: 0,
                root: None,
                project,
                ended: None,
                generation: 0,
                last_used,
                own_events: RecentIds::new(OWN_EVENT_CAP),
                streamed: String::new(),
                deferred: Vec::new(),
                deferred_dropped: 0,
                last_prompt: None,
                pending: None,
            },
        );
        self.send(wire::ok(id, json!({ "sessionId": session_id })))
            .await
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Drop the least recently used session without a running prompt.
    /// Returns false when every session has one.
    fn evict_idle_session(&mut self) -> bool {
        let oldest = self
            .sessions
            .values()
            .filter(|s| s.pending.is_none())
            .min_by_key(|s| s.last_used)
            .map(|s| s.id.clone());
        oldest.is_some_and(|id| self.sessions.remove(&id).is_some())
    }

    async fn on_prompt(&mut self, id: Value, params: &Value) -> Result<(), CliError> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = match prompt::parse(params.get("prompt")) {
            Ok(text) => text,
            Err(message) => return self.respond_error(id, wire::INVALID_PARAMS, &message).await,
        };
        let Some(session) = self.sessions.get_mut(session_id) else {
            return self
                .respond_error(
                    id,
                    wire::INVALID_PARAMS,
                    &format!("unknown session: {session_id}"),
                )
                .await;
        };
        if session.pending.is_some() {
            return self
                .respond_error(
                    id,
                    wire::INVALID_REQUEST,
                    "a prompt is already running in this session",
                )
                .await;
        }
        if let Some(error) = &self.relay_error {
            let message = error.clone();
            return self.respond_error(id, wire::INTERNAL_ERROR, &message).await;
        }
        if session.ended.is_some() {
            return self.on_ended_prompt(id, session_id, text).await;
        }
        self.begin_prompt(id, session_id, text.clone(), text);
        Ok(())
    }

    /// Answer a prompt to an ended session: continue on `/recover` or
    /// `/fresh`, otherwise repeat the choices without posting anything.
    async fn on_ended_prompt(
        &mut self,
        id: Value,
        session_id: &str,
        text: Prompt,
    ) -> Result<(), CliError> {
        let channel = self.scope.channel;
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Ok(());
        };
        let command = parse_recovery(&text.text);
        let body = match command {
            Some((_, rest)) if !rest.is_empty() || !text.images.is_empty() => Some(Prompt {
                text: rest.to_owned(),
                ..text.clone()
            }),
            Some(_) => session.last_prompt.clone(),
            None => None,
        };
        let (Some((command, _)), Some(body)) = (command, body) else {
            self.show_ended(session_id).await?;
            return self
                .send(wire::ok(id, json!({ "stopReason": "end_turn" })))
                .await;
        };
        session.ended = None;
        let posted = match command {
            Recovery::SameThread => body.clone(),
            Recovery::NewThread => match session.root.take() {
                Some(old_root) => Prompt {
                    text: format!(
                        "{}\n\nPrevious thread (its agent session ended): {}",
                        body.text,
                        message_link(channel, &old_root)
                    ),
                    ..body.clone()
                },
                None => body.clone(),
            },
        };
        self.send(wire::session_update(
            session_id,
            wire::available_commands(&[]),
        ))
        .await?;
        self.begin_prompt(id, session_id, posted, body);
        Ok(())
    }

    /// Tell the user the session ended, why, and offer `/fresh`, plus
    /// `/recover` when the manager's notice invites a mention here.
    async fn show_ended(&self, session_id: &str) -> Result<(), CliError> {
        let session = self.sessions.get(session_id);
        let root = session.and_then(|s| s.root.as_deref()).unwrap_or_default();
        let reason = session.and_then(|s| s.ended.as_deref()).unwrap_or_default();
        let notice = ended_notice(reason, &message_link(self.scope.channel, root));
        self.send(wire::session_update(
            session_id,
            wire::agent_text_chunk(&notice),
        ))
        .await?;
        self.send(wire::session_update(
            session_id,
            wire::available_commands(&ended_commands(reason)),
        ))
        .await
    }

    /// Start a prompt that posts `posted`; `user_text` is what a later
    /// `/recover` or `/fresh` resends.
    fn begin_prompt(&mut self, id: Value, session_id: &str, posted: Prompt, user_text: Prompt) {
        let tick = self.next_tick();
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        let prompt = posted;
        session.last_used = tick;
        session.last_prompt = Some(user_text);
        session.generation += 1;
        session.streamed.clear();
        session.deferred.clear();
        session.pending = Some(PendingPrompt {
            request_id: id,
            generation: session.generation,
            prompt,
            phase: Phase::AwaitingRelay,
            event_id: None,
            created_at: None,
            is_root: false,
            turn_id: None,
            stop_reason: None,
            usage: None,
            cancel: None,
            fallback_deadline: None,
            silence_deadline: None,
            prepare: None,
        });
        if self.relay_live {
            self.start_prepare(session_id.to_owned(), Duration::ZERO);
        }
    }

    fn start_waiting_prompts(&mut self) {
        let waiting: Vec<String> = self
            .sessions
            .values()
            .filter(|s| {
                s.pending
                    .as_ref()
                    .is_some_and(|p| p.phase == Phase::AwaitingRelay)
            })
            .map(|s| s.id.clone())
            .collect();
        for session_id in waiting {
            self.start_prepare(session_id, Duration::ZERO);
        }
    }

    /// Sign the pending prompt in the background, after `delay`.
    fn start_prepare(&mut self, session_id: String, delay: Duration) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let root = session.root.clone();
        let Some(pending) = session.pending.as_mut() else {
            return;
        };
        pending.phase = Phase::Preparing;
        let generation = pending.generation;
        let prompt = std::mem::take(&mut pending.prompt);
        let relay = Arc::clone(&self.relay);
        let ops = self.ops.clone();
        let prepares = Arc::clone(&self.prepares);
        let task = tokio::spawn(async move {
            // Wait for a slot: every holder is bounded by the timeout below,
            // and a cancel while preparing ends the prompt, whose late result
            // the generation check then discards.
            let result = match prepares.acquire_owned().await {
                Ok(_permit) => {
                    tokio::time::sleep(delay).await;
                    tokio::time::timeout(
                        Duration::from_secs(120),
                        relay.prepare_content(prompt, root),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(CliError::Other(
                            "image/message preparation timed out; prompt was not sent".into(),
                        ))
                    })
                }
                Err(_) => Err(CliError::Other(
                    "message preparation stopped; retry this prompt".into(),
                )),
            };
            let _ = ops
                .send(Op::Prepared {
                    session: session_id,
                    generation,
                    result,
                })
                .await;
        });
        pending.prepare = Some(PrepareTask(task.abort_handle()));
    }

    async fn on_cancel(&mut self, params: &Value) -> Result<(), CliError> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Ok(());
        };
        let Some(pending) = session.pending.as_mut() else {
            return Ok(());
        };
        if pending.cancel.is_some() {
            return Ok(());
        }
        if matches!(pending.phase, Phase::AwaitingRelay | Phase::Preparing) {
            // Nothing reached the relay; a late Prepared result is fenced out
            // because the pending prompt is gone.
            return self.finish(session_id, "cancelled").await;
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let generation = pending.generation;
        pending.cancel = Some(CancelState {
            request_id: request_id.clone(),
            deadline: Instant::now() + self.timing.cancel_wait,
        });
        let frame = match frames::cancel_turn_frame(&self.scope, &request_id) {
            Ok(frame) => frame,
            Err(e) => return self.fail(session_id, &cancel_failed(&e)).await,
        };
        let relay = Arc::clone(&self.relay);
        let ops = self.ops.clone();
        let session = session_id.to_owned();
        tokio::spawn(async move {
            let result = relay.publish_ephemeral(frame).await;
            let _ = ops
                .send(Op::CancelPublished {
                    session,
                    generation,
                    result,
                })
                .await;
        });
        Ok(())
    }

    // ---- background results --------------------------------------------

    /// The pending prompt of `session_id`, if it is still `generation`.
    fn current(&mut self, session_id: &str, generation: u64) -> Option<&mut Session> {
        self.sessions
            .get_mut(session_id)
            .filter(|s| s.pending.as_ref().map(|p| p.generation) == Some(generation))
    }

    async fn on_op(&mut self, op: Op) -> Result<(), CliError> {
        match op {
            Op::PeerNameWarmed => {
                self.peer_names = self.peer_names.saturating_sub(1);
                Ok(())
            }

            Op::HistoryListed { id, result } => {
                self.history_ops = self.history_ops.saturating_sub(1);
                self.finish_history_list(id, result).await
            }
            Op::HistoryLoaded {
                id,
                session,
                token,
                prior,
                result,
            } => {
                self.history_ops = self.history_ops.saturating_sub(1);
                let result = self
                    .finish_history_load(id, session, token, prior, result)
                    .await;
                if self.loading.is_empty() {
                    self.history_live.clear();
                    self.history_live_bytes = 0;
                }
                result
            }
            Op::Prepared {
                session,
                generation,
                result,
            } => {
                let signer = self.scope.signer.clone();
                let Some(state) = self.current(&session, generation) else {
                    return Ok(());
                };
                let mut event = match result {
                    Ok(event) => event,
                    Err(e) => {
                        return self
                            .fail(
                                &session,
                                &format!("could not prepare the Buzz message: {e}"),
                            )
                            .await
                    }
                };
                if state.root.is_none() {
                    let mut tags = event.tags.to_vec();
                    let mut added = vec![["d".to_owned(), format!("buzz-acp:{session}")]];
                    if let Some(project) = &state.project {
                        added.push([projects::PROJECT_TAG.to_owned(), project.clone()]);
                    }
                    match added
                        .into_iter()
                        .map(nostr::Tag::parse)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| CliError::Other(e.to_string()))
                        .and_then(|added| {
                            tags.extend(added);
                            nostr::EventBuilder::new(event.kind, event.content.clone())
                                .tags(tags)
                                .custom_created_at(event.created_at)
                                .sign_with_keys(&signer)
                                .map_err(|e| CliError::Other(e.to_string()))
                        }) {
                        Ok(signed) => event = signed,
                        Err(error) => {
                            return self
                                .fail(
                                    &session,
                                    &format!("could not persist ACP session identity: {error}"),
                                )
                                .await
                        }
                    }
                }
                let prepared = Prompt::from_event(&event);
                // `/recover` and `/fresh` resend the user's own text, not the
                // posted one (which can carry an old-thread link), with the
                // uploaded copies of its images instead of the raw bytes.
                if let Some(last) = state.last_prompt.as_mut() {
                    if !last.images.is_empty() {
                        last.images.clear();
                        last.media = prepared.media.clone();
                    }
                }
                if let Some(pending) = state.pending.as_mut() {
                    pending.prompt = prepared;
                }
                if state.own_events.contains(&event.id) {
                    // Same text, thread and second as an earlier prompt (a
                    // quick `/recover`): the relay would take it as a
                    // duplicate and nothing would answer. Sign it again once
                    // the timestamp has moved on.
                    self.start_prepare(session, DUPLICATE_RESIGN_DELAY);
                    return Ok(());
                }
                let event_id = event.id.to_hex();
                state.own_events.insert(event.id);
                let is_root = state.root.is_none();
                if is_root {
                    state.root = Some(event_id.clone());
                }
                let root = is_root.then(|| event_id.clone());
                if let Some(pending) = state.pending.as_mut() {
                    pending.event_id = Some(event_id);
                    pending.created_at = Some(event.created_at);
                    pending.is_root = is_root;
                    pending.phase = Phase::Submitting;
                }
                let relay = Arc::clone(&self.relay);
                let ops = self.ops.clone();
                tokio::spawn(async move {
                    let result = relay.submit(event).await;
                    let _ = ops
                        .send(Op::Submitted {
                            session,
                            generation,
                            root,
                            result,
                        })
                        .await;
                });
                Ok(())
            }
            Op::Submitted {
                session,
                generation,
                root,
                result,
            } => {
                // Reconcile the root even when the prompt is already gone, for
                // example failed by an unreachable relay while submitting. A
                // definite refusal means the relay never stored it, so the next
                // prompt starts a new thread. An unknown delivery may have
                // stored it, so later prompts keep replying to it.
                if let (Err(error), Some(root)) = (&result, &root) {
                    if !matches!(error, CliError::DeliveryUnknown(_)) {
                        if let Some(state) = self.sessions.get_mut(&session) {
                            if state.root.as_deref() == Some(root.as_str()) {
                                state.root = None;
                            }
                        }
                    }
                }
                let Some(state) = self.current(&session, generation) else {
                    return Ok(());
                };
                match result {
                    Ok(()) => {
                        if let Some(pending) = state.pending.as_mut() {
                            pending.phase = Phase::Posted;
                        }
                        Ok(())
                    }
                    Err(e) => {
                        self.fail(&session, &format!("could not post to Buzz: {e}"))
                            .await
                    }
                }
            }
            Op::CancelPublished {
                session,
                generation,
                result,
            } => {
                if self.current(&session, generation).is_none() {
                    return Ok(());
                }
                match result {
                    Ok(()) => Ok(()),
                    Err(e) => self.fail(&session, &cancel_failed(&e)).await,
                }
            }
        }
    }

    // ---- relay events --------------------------------------------------

    async fn on_relay_event(&mut self, event: Event) -> Result<(), CliError> {
        if !self.seen.insert(event.id) {
            return Ok(());
        }
        let kind = u32::from(event.kind.as_u16());
        if kind == buzz_core::kind::KIND_AGENT_OBSERVER_FRAME {
            match frames::decode_telemetry(&self.scope, &event) {
                Ok(events) => {
                    for observer_event in events {
                        self.on_signal(frames::signal_of(observer_event)).await?;
                    }
                }
                // Control frames and other agents' telemetry are expected noise.
                Err(frames::FrameError::NotTelemetry | frames::FrameError::WrongAgent) => {}
                Err(e) => eprintln!("acp-bridge: rejected observer frame {}: {e:?}", event.id),
            }
            return Ok(());
        }
        if kind == buzz_core::kind::KIND_STREAM_MESSAGE
            || kind == buzz_core::kind::KIND_STREAM_MESSAGE_V2
        {
            if self.loading.values().any(|load| !load.overflowed) {
                let bytes = self.history_live_bytes.saturating_add(
                    serde_json::to_vec(&event)
                        .map_err(|error| CliError::Other(error.to_string()))?
                        .len(),
                );
                if self.history_live.len() >= 1024 || bytes > HISTORY_LIVE_BYTES {
                    // Only snapshots overlapping this gap are invalid. Keep the
                    // bridge and its existing prompt/cancellation paths alive.
                    for load in self.loading.values_mut() {
                        load.overflowed = true;
                    }
                    self.history_live.clear();
                    self.history_live_bytes = 0;
                } else {
                    self.history_live_bytes = bytes;
                    self.history_live.push(event.clone());
                }
            }
            return self.on_thread_message(&event).await;
        }
        Ok(())
    }

    /// Find the session whose current prompt is running `turn_id`.
    fn session_for_turn(&self, turn_id: &str) -> Option<String> {
        self.sessions
            .values()
            .find(|s| {
                s.pending
                    .as_ref()
                    .and_then(|p| p.turn_id.as_deref())
                    .is_some_and(|t| t == turn_id)
            })
            .map(|s| s.id.clone())
    }

    async fn on_signal(&mut self, signal: Signal) -> Result<(), CliError> {
        match signal {
            Signal::TurnStarted {
                turn_id,
                triggering,
            } => {
                // Bind the turn to the prompt it answers. Turns for other
                // prompts or threads stay unbound and their frames are dropped.
                for session in self.sessions.values_mut() {
                    if let Some(pending) = session.pending.as_mut() {
                        let answers_prompt = pending
                            .event_id
                            .as_ref()
                            .is_some_and(|id| triggering.contains(id));
                        if answers_prompt && pending.turn_id.is_none() {
                            pending.turn_id = Some(turn_id.clone());
                            pending.fallback_deadline = None;
                        }
                    }
                }
                Ok(())
            }
            Signal::CancelResult { request_id, status } => {
                let target = self.sessions.values().find_map(|s| {
                    let cancel = s.pending.as_ref()?.cancel.as_ref()?;
                    (cancel.request_id == request_id).then(|| s.id.clone())
                });
                let Some(session_id) = target else {
                    return Ok(());
                };
                if status == "sent" {
                    // The harness signalled the turn; wait for turn_completed.
                    return Ok(());
                }
                eprintln!("acp-bridge: agent answered cancel_turn with {status}");
                self.finish(&session_id, "cancelled").await
            }
            Signal::TurnCompleted { turn_id } => {
                let Some(session_id) = self.session_for_turn(&turn_id) else {
                    return Ok(());
                };
                let stop_reason = self
                    .sessions
                    .get(&session_id)
                    .and_then(|s| s.pending.as_ref())
                    .map(|p| {
                        if p.cancel.is_some() {
                            "cancelled".to_owned()
                        } else {
                            p.stop_reason.clone().unwrap_or_else(|| "end_turn".into())
                        }
                    })
                    .unwrap_or_else(|| "end_turn".into());
                self.finish(&session_id, &stop_reason).await
            }
            Signal::Liveness { turn_id } => {
                self.touch_turn(&turn_id, true);
                Ok(())
            }
            Signal::StopReason {
                turn_id,
                reason,
                usage,
            } => {
                if let Some(pending) = self.touch_turn(&turn_id, false) {
                    pending.stop_reason = Some(reason);
                    pending.usage = usage;
                }
                Ok(())
            }
            Signal::Update { turn_id, update } => {
                let Some(session_id) = self.session_for_turn(&turn_id) else {
                    return Ok(());
                };
                self.touch_turn(&turn_id, false);
                if update.get("sessionUpdate").and_then(Value::as_str)
                    == Some("agent_message_chunk")
                {
                    let text = update
                        .get("content")
                        .and_then(|content| content.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some(session) = self.sessions.get_mut(&session_id) {
                        if session.streamed.len() + text.len() <= STREAMED_TEXT_CAP {
                            session.streamed.push_str(text);
                        }
                    }
                }
                self.send(wire::session_update(&session_id, update)).await
            }
            Signal::Ignored => Ok(()),
        }
    }

    /// Record activity on `turn_id`. Liveness frames arm the silence timeout;
    /// any later frame of the turn extends it.
    fn touch_turn(&mut self, turn_id: &str, liveness: bool) -> Option<&mut PendingPrompt> {
        let silence = self.timing.turn_silence;
        let pending = self
            .sessions
            .values_mut()
            .filter_map(|s| s.pending.as_mut())
            .find(|p| p.turn_id.as_deref() == Some(turn_id))?;
        if liveness || pending.silence_deadline.is_some() {
            pending.silence_deadline = Some(Instant::now() + silence);
        }
        Some(pending)
    }

    async fn on_thread_message(&mut self, event: &Event) -> Result<(), CliError> {
        let channel = self.scope.channel.to_string();
        if event
            .tags
            .iter()
            .find(|t| t.as_slice().first().map(String::as_str) == Some("h"))
            .and_then(|t| t.as_slice().get(1))
            != Some(&channel)
        {
            return Ok(());
        }
        let Some((root, _)) = buzz_core::nip10::parse_thread_markers(&event.tags).resolve() else {
            return Ok(());
        };
        let Some(session) = self
            .sessions
            .values()
            .find(|s| s.root.as_deref() == Some(root.as_str()))
        else {
            return Ok(());
        };
        if session.own_events.contains(&event.id) {
            return Ok(());
        }
        let session_id = session.id.clone();
        if event.pubkey != self.scope.agent {
            let who = if event.pubkey == self.scope.signer.public_key()
                || event.pubkey == self.scope.owner
            {
                "you, from another client".to_owned()
            } else {
                let cached = self.names.cached(event.pubkey);
                if cached.is_none() && self.peer_names < 16 {
                    self.peer_names += 1;
                    let (relay, names, ops, author) = (
                        Arc::clone(&self.relay),
                        Arc::clone(&self.names),
                        self.ops.clone(),
                        event.pubkey,
                    );
                    tokio::spawn(async move {
                        names.resolve(relay.as_ref(), [author]).await;
                        // Warming is presentation-only: a late completion must
                        // never deliver text into this or a reloaded session.
                        let _ = ops.send(Op::PeerNameWarmed).await;
                    });
                }
                cached.unwrap_or_else(|| names::fallback(event.pubkey))
            };
            let text = format!("\n\n[{who}] {}\n\n", event.content);
            return self
                .send(wire::session_update(
                    &session_id,
                    wire::agent_text_chunk(&text),
                ))
                .await;
        }

        if event.content.starts_with(SESSION_ENDED_NOTICE) {
            return self.on_session_ended(&session_id, event).await;
        }

        let grace = self.timing.no_telemetry_grace;
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Ok(());
        };
        if let Some(pending) = session.pending.as_mut() {
            let after_prompt = pending
                .created_at
                .is_some_and(|prompted| event.created_at >= prompted);
            if pending.turn_id.is_some() {
                // Telemetry is live for this turn and may still stream this
                // text; hold the reply briefly so trailing telemetry can
                // arrive, then show it without waiting for the turn to end.
                if streamed_contains(&session.streamed, &event.content) {
                    return Ok(());
                }
                if session.deferred.len() < DEFERRED_REPLY_CAP {
                    let show_at = Instant::now() + self.timing.live_reply_delay;
                    session.deferred.push((event.content.clone(), show_at));
                } else {
                    // Sending it now could duplicate text telemetry streams
                    // later, so count it and point to the thread at turn end.
                    session.deferred_dropped = session.deferred_dropped.saturating_add(1);
                    eprintln!(
                        "acp-bridge: more than {DEFERRED_REPLY_CAP} agent replies in one turn; the rest stay in the Buzz thread"
                    );
                }
                return Ok(());
            }
            // Without telemetry, an agent reply after this prompt ends it once
            // the grace period passes with no turn_started to bind the turn.
            if after_prompt && pending.cancel.is_none() && pending.fallback_deadline.is_none() {
                pending.fallback_deadline = Some(Instant::now() + grace);
            }
        }
        if streamed_contains(&session.streamed, &event.content) {
            // Telemetry already streamed this text.
            return Ok(());
        }
        self.send(wire::session_update(
            &session_id,
            wire::agent_text_chunk(&event.content),
        ))
        .await
    }

    /// The sandbox manager's reply that this thread's session ended. Acts
    /// only on the notice for the running prompt, or, with no prompt running,
    /// for someone else's mention; a late notice for an earlier prompt of
    /// this session is stale.
    async fn on_session_ended(&mut self, session_id: &str, event: &Event) -> Result<(), CliError> {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Ok(());
        };
        let trigger = buzz_core::nip10::parse_thread_markers(&event.tags)
            .resolve()
            .map(|(_, parent)| parent);
        let for_pending = session
            .pending
            .as_ref()
            .is_some_and(|p| p.event_id.is_some() && p.event_id == trigger);
        let stale = trigger
            .as_deref()
            .and_then(|id| EventId::from_hex(id).ok())
            .is_some_and(|id| session.own_events.contains(&id));
        if !for_pending && (stale || session.pending.is_some()) {
            return Ok(());
        }
        session.ended = Some(event.content.chars().take(ENDED_REASON_CHARS).collect());
        self.show_ended(session_id).await?;
        if for_pending {
            return self.finish(session_id, "end_turn").await;
        }
        Ok(())
    }

    // ---- deadlines and completion ----------------------------------------

    fn next_deadline(&self) -> Option<Instant> {
        let replies = self
            .sessions
            .values()
            .flat_map(|s| s.deferred.iter().map(|(_, show_at)| *show_at));
        self.sessions
            .values()
            .filter_map(|s| s.pending.as_ref())
            .flat_map(|p| {
                [
                    p.fallback_deadline,
                    p.silence_deadline,
                    p.cancel.as_ref().map(|c| c.deadline),
                ]
            })
            .flatten()
            .chain(replies)
            .min()
    }

    /// Show held agent replies whose delay has passed, except those whose
    /// text telemetry streamed meanwhile.
    async fn release_replies(&mut self, now: Instant) -> Result<(), CliError> {
        let mut due = Vec::new();
        for session in self.sessions.values_mut() {
            let (ready, held): (Vec<_>, Vec<_>) = std::mem::take(&mut session.deferred)
                .into_iter()
                .partition(|(_, show_at)| *show_at <= now);
            session.deferred = held;
            for (text, _) in ready {
                if !streamed_contains(&session.streamed, &text) {
                    due.push((session.id.clone(), text));
                }
            }
        }
        for (session_id, text) in due {
            self.send(wire::session_update(
                &session_id,
                wire::agent_text_chunk(&format!("\n\n{text}\n\n")),
            ))
            .await?;
        }
        Ok(())
    }

    async fn on_deadlines(&mut self) -> Result<(), CliError> {
        let now = Instant::now();
        self.release_replies(now).await?;
        let mut expired = Vec::new();
        for session in self.sessions.values() {
            let Some(pending) = session.pending.as_ref() else {
                continue;
            };
            let due = |deadline: Option<Instant>| deadline.is_some_and(|d| d <= now);
            if due(pending.cancel.as_ref().map(|c| c.deadline)) {
                expired.push((session.id.clone(), Expiry::Cancel));
            } else if due(pending.silence_deadline) {
                expired.push((session.id.clone(), Expiry::Silence));
            } else if due(pending.fallback_deadline) {
                expired.push((session.id.clone(), Expiry::NoTelemetry));
            }
        }
        for (session_id, expiry) in expired {
            match expiry {
                Expiry::Cancel => self.finish(&session_id, "cancelled").await?,
                Expiry::NoTelemetry => {
                    eprintln!(
                        "acp-bridge: no telemetry for this turn; ending it after the agent's reply"
                    );
                    self.finish(&session_id, "end_turn").await?;
                }
                Expiry::Silence => {
                    let message = format!(
                        "lost contact with the agent's turn: no activity for {}s. It may still be running; check the thread in Buzz.",
                        self.timing.turn_silence.as_secs()
                    );
                    self.fail(&session_id, &message).await?;
                }
            }
        }
        Ok(())
    }

    /// Emit the agent replies held during the turn, except those whose text
    /// telemetry streamed.
    async fn flush_deferred(&mut self, session_id: &str) -> Result<(), CliError> {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Ok(());
        };
        let mut deferred: Vec<String> = std::mem::take(&mut session.deferred)
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        deferred.retain(|text| !streamed_contains(&session.streamed, text));
        let dropped = std::mem::take(&mut session.deferred_dropped);
        if dropped > 0 {
            deferred.push(format!(
                "\n\n[{dropped} more agent replies from this turn are only in the Buzz thread]\n\n"
            ));
        }
        for text in deferred {
            self.send(wire::session_update(
                session_id,
                wire::agent_text_chunk(&text),
            ))
            .await?;
        }
        Ok(())
    }

    /// Answer the pending prompt with `stop_reason` and, when the remote turn
    /// reported one, its token `usage`.
    async fn finish(&mut self, session_id: &str, stop_reason: &str) -> Result<(), CliError> {
        self.stop_preparation(session_id);
        self.flush_deferred(session_id).await?;
        let Some(pending) = self
            .sessions
            .get_mut(session_id)
            .and_then(|s| s.pending.take())
        else {
            return Ok(());
        };
        let mut result = json!({ "stopReason": stop_reason });
        if let Some(usage) = pending.usage {
            result["usage"] = usage;
        }
        self.send(wire::ok(pending.request_id, result)).await
    }

    /// Stop the pending prompt's preparation before any output is awaited: a
    /// slot freed while the answer waits for stdout must not let it start.
    fn stop_preparation(&mut self, session_id: &str) {
        if let Some(pending) = self
            .sessions
            .get_mut(session_id)
            .and_then(|session| session.pending.as_mut())
        {
            pending.prepare = None;
        }
    }

    /// Answer the pending prompt with an error.
    async fn fail(&mut self, session_id: &str, message: &str) -> Result<(), CliError> {
        self.stop_preparation(session_id);
        self.flush_deferred(session_id).await?;
        let Some(pending) = self
            .sessions
            .get_mut(session_id)
            .and_then(|s| s.pending.take())
        else {
            return Ok(());
        };
        self.respond_error(pending.request_id, wire::INTERNAL_ERROR, message)
            .await
    }

    async fn fail_all_prompts(&mut self, message: &str) -> Result<(), CliError> {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for session_id in ids {
            self.fail(&session_id, message).await?;
        }
        Ok(())
    }
}

enum Expiry {
    Cancel,
    Silence,
    NoTelemetry,
}

/// Whether `reply` repeats text already in `streamed`, ignoring whitespace
/// differences such as a heredoc's trailing newline.
fn streamed_contains(streamed: &str, reply: &str) -> bool {
    let squash = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let reply = squash(reply);
    !reply.is_empty() && squash(streamed).contains(&reply)
}

/// A `buzz://` deep link to a message in `channel`.
pub(crate) fn message_link(channel: uuid::Uuid, event_id: &str) -> String {
    format!("buzz://message?channel={channel}&id={event_id}")
}

/// Whether the sandbox manager's ended notice (`reason`) leaves a mention in
/// this thread as a way to restore the session: it invites a mention here
/// and does not say the checkpoint cannot be restored. A notice that only
/// points to a new thread (no saved checkpoint, or a manager that cannot
/// restore on a mention) gives `false`.
pub(crate) fn mention_may_restore(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("me here") && !reason.contains("cannot be restored")
}

/// The commands offered after the thread's session ended, for the
/// manager's `reason`. `/recover` stays accepted either way.
fn ended_commands(reason: &str) -> Vec<(&'static str, &'static str)> {
    let fresh = (
        FRESH_COMMAND.trim_start_matches('/'),
        "Resend your message as a new thread, in a new session with an empty workspace",
    );
    if !mention_may_restore(reason) {
        return vec![fresh];
    }
    vec![
        (
            RECOVER_COMMAND.trim_start_matches('/'),
            "Resend your message in this thread; the new mention lets the agent's manager try to restore the session",
        ),
        fresh,
    ]
}

/// What the user can do after the thread's session ended. The bridge signs
/// with the owner's (or a delegated gateway) key. `/recover` resends the
/// message with a new mention of the agent in this thread, so the sandbox
/// manager can try to restore the session; it is offered only when the
/// manager's `reason` invites that (see [`mention_may_restore`]).
fn ended_notice(reason: &str, thread_link: &str) -> String {
    let fresh = format!(
        "- Reply `{FRESH_COMMAND}` to resend it as a new thread. The agent starts a new session with an empty workspace."
    );
    let choices = if mention_may_restore(reason) {
        format!(
            "- Reply `{RECOVER_COMMAND}` to resend it in this thread ({thread_link}). \
             That mentions the agent here again, so its manager can try to restore the session.\n\
             {fresh}\n\n\
             Add text after either command to send that text instead."
        )
    } else {
        format!("{fresh}\n\nAdd text after the command to send that text instead.")
    };
    format!(
        "This thread's agent session has ended, so the agent did not get your message.\n\n\
         > {reason}\n\n\
         {choices}"
    )
}

fn cancel_failed(error: &CliError) -> String {
    format!(
        "could not deliver the cancel request to the agent ({error}); the turn may still be running in Buzz"
    )
}
