//! One `claude` process driven over its stream-json protocol, a port of
//! buzz-app's `claude.ts`: the protocol the Claude Agent SDK speaks to the
//! same binary, so no Node runtime is needed. The process stays alive
//! between turns; messages sent while it works are folded into the running
//! turn at its next tool call. Buzz tools are served over the same pipe, as
//! the SDK serves an in-process MCP server, so the process holds no Buzz
//! key.

use crate::agent_activity::local::WorkHandle;
use crate::agent_plugins::process::{Lines, Process, SpawnOptions, Spawner};
use buzz_host::api::ProcessOutput;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

/// Its tools are `mcp__buzz__<name>`.
pub const TOOL_SERVER: &str = "buzz";
const FINAL_TOOL: &str = "mcp__buzz__send";
const STDERR_LIMIT: usize = 4_000;
/// How long Claude may stay idle with a message it has not taken before
/// the messages are settled anyway.
const UNREAD_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(3)
};
/// The longest line of output read. Tool results with images run to a
/// few megabytes.
const FRAME_LIMIT: usize = 64 * 1024 * 1024;

/// Answers the in-process server's JSON-RPC for the conversation a process
/// serves. A call of `send` with `final: true` ends the turn when it
/// succeeds.
pub type ToolServer =
    Arc<dyn Fn(Option<String>, Value) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;

#[derive(Clone, Default)]
pub struct Launch {
    pub cwd: String,
    /// Appended to Claude's own system prompt.
    pub system_prompt: String,
    pub model: Option<String>,
    /// A saved session to continue.
    pub resume: Option<String>,
    /// The id a new session takes, so it is known before its first turn.
    pub session_id: Option<String>,
    /// The in-process MCP server's tools.
    pub tools: Option<ToolServer>,
}

/// A model Claude lists as one a session can switch to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeModel {
    /// What `set_model` takes: an alias such as `opus`, or a model id.
    pub value: String,
    pub resolved_model: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub supports_effort: Option<bool>,
    pub supported_effort_levels: Option<Vec<String>>,
}

/// What the next turn is asked to run with; `None` is the session's own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnPick {
    pub model: Option<String>,
    pub effort: Option<String>,
}

type Listed = Shared<BoxFuture<'static, Vec<ClaudeModel>>>;

/// How a burst of work ended: every message sent so far has been answered,
/// or the process is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    pub ok: bool,
    /// Why it failed, for the person waiting.
    pub error: Option<String>,
}

impl Settled {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
        }
    }
}

/// What `claude` is started with: as the SDK starts it, reporting when it
/// is idle, signed in as `claude auth login` left it, and never as part of
/// a Claude session that started the app.
pub fn env() -> BTreeMap<String, Option<String>> {
    let mut env = BTreeMap::new();
    env.insert("CLAUDE_CODE_ENTRYPOINT".into(), Some("sdk-ts".into()));
    env.insert(
        "CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS".into(),
        Some("1".into()),
    );
    // Buzz keeps its own copy current; Claude must not install itself
    // elsewhere.
    env.insert("DISABLE_AUTOUPDATER".into(), Some("1".into()));
    // A key in the shell that launched the app would outrank the sign-in;
    // the interactive CLI asks before using one, but stream-json mode uses
    // it silently.
    for name in [
        "ANTHROPIC_API_KEY",
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_CODE_SESSION_ATTENDED",
        "CLAUDE_AGENT_SDK_VERSION",
        "CLAUDE_PID",
    ] {
        env.insert(name.into(), None);
    }
    env
}

/// The arguments `claude` is started with.
pub fn args(launch: &Launch) -> Vec<String> {
    let mut args: Vec<String> = [
        "--output-format",
        "stream-json",
        "--input-format",
        "stream-json",
        "--verbose",
        // Echoes each message as Claude takes it, so a turn that ends just
        // as one arrives is not taken for the answer to it.
        "--replay-user-messages",
        "--permission-mode",
        "bypassPermissions",
    ]
    .map(String::from)
    .to_vec();
    if let Some(model) = &launch.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(resume) = &launch.resume {
        args.extend(["--resume".into(), resume.clone()]);
    } else if let Some(id) = &launch.session_id {
        args.extend(["--session-id".into(), id.clone()]);
    }
    if launch.tools.is_some() {
        // Listed up front, so Claude need not search for them first.
        let config = json!({
            "mcpServers": {
                TOOL_SERVER: { "type": "sdk", "name": TOOL_SERVER, "alwaysLoad": true },
            },
        });
        args.extend(["--mcp-config".into(), config.to_string()]);
    }
    args
}

struct State {
    /// Its session, known once the first turn starts (or at once when
    /// resuming).
    session_id: Option<String>,
    /// The conversation it serves, set when a spare is put to use.
    conversation: Option<String>,
    /// Messages sent and not yet answered.
    pending: usize,
    /// Messages sent that Claude has not echoed back as taken yet.
    unread: usize,
    /// Bumped on every reported state change, so a late check can tell
    /// whether Claude moved on.
    state_changes: u64,
    /// It has begun a turn, so its session loaded.
    began: bool,
    waiters: Vec<oneshot::Sender<Settled>>,
    buffer: Lines,
    stderr: String,
    last_error: Option<String>,
    alive: bool,
    /// It reports idle; an older `claude` that does not is settled per
    /// result.
    reports_state: bool,
    requests: u64,
    /// Control requests awaiting Claude's answer, by request id.
    calls: HashMap<String, oneshot::Sender<Value>>,
    /// The models Claude lists, from its answer to `initialize`.
    listed: Listed,
    /// The model the session reported when it began.
    reported: Option<String>,
    /// The model the next turn runs on; `None` is the session's own.
    picked_model: Option<String>,
    /// The effort the next turn runs at; `None` is the session's own.
    effort: Option<String>,
    /// The session's own effort, read before the first change to restore
    /// it.
    own_effort: Option<String>,
    reading_effort: Option<Shared<BoxFuture<'static, ()>>>,
    /// Messages sent with a work row, by the uuid Claude echoes back.
    receipts: HashMap<String, Receipt>,
    /// The latest acknowledged mention per Buzz thread: the one that gets
    /// steps.
    active: HashMap<String, Receipt>,
}

/// A mention's work row, sent with its message so Claude's echo and
/// steps reach it (`ActivityReceipt`).
#[derive(Clone)]
pub struct Receipt {
    /// Its Buzz thread: a session shared by a channel handles several.
    pub scope: String,
    pub work: WorkHandle,
}

struct Inner {
    process: Process,
    /// The session's own model, as launched; `None` is Claude's
    /// default.
    own_model: Option<String>,
    /// Replaced when a spare is given to the agent that takes it.
    tools: Mutex<Option<ToolServer>>,
    state: Mutex<State>,
    /// Lines for stdin, written one at a time in the order sent.
    writes: mpsc::UnboundedSender<(String, oneshot::Sender<Result<(), String>>)>,
    exited: watch::Sender<Option<Option<i32>>>,
}

/// A running `claude`. Cheap to clone; clones are the same process.
#[derive(Clone)]
pub struct ClaudeProcess {
    inner: Arc<Inner>,
}

impl ClaudeProcess {
    pub async fn start(spawn: &Spawner, launch: Launch) -> Result<Self, String> {
        let claude = Self::spawn(spawn, &launch).await?;
        claude.initialize(&launch).await?;
        Ok(claude)
    }

    /// Starts `claude` without its prompt, which `launch` may leave empty.
    /// This is most of what a start costs, and fixes nothing about the agent
    /// it serves but its folder and model.
    pub async fn spawn(spawn: &Spawner, launch: &Launch) -> Result<Self, String> {
        let spawned = spawn
            .spawn(
                "claude",
                SpawnOptions {
                    args: args(launch),
                    cwd: Some(launch.cwd.clone()),
                    env: env(),
                },
            )
            .await?;
        Ok(Self::attach(
            spawned.process,
            spawned.output,
            launch.resume.clone().or(launch.session_id.clone()),
            launch.tools.clone(),
            launch.model.clone(),
        ))
    }

    /// Gives a started process its prompt, and its tools to whoever it now
    /// works for. Loads settings and tools now rather than on the first
    /// message.
    pub async fn initialize(&self, launch: &Launch) -> Result<(), String> {
        if let Some(tools) = &launch.tools {
            *self.tools() = Some(tools.clone());
        }
        let mut initialize = json!({
            "subtype": "initialize",
            "appendSystemPrompt": launch.system_prompt,
        });
        if launch.tools.is_some() {
            initialize["sdkMcpServers"] = json!([TOOL_SERVER]);
            initialize["hooks"] = json!({
                "PostToolUse": [{ "matcher": FINAL_TOOL, "hookCallbackIds": ["final"] }],
            });
        }
        let (written, answer) = self.request(initialize);
        written.await?;
        self.state().listed = async move {
            answer
                .await
                .ok()
                .and_then(|response| {
                    let models = response.get("response")?.get("models")?.as_array()?.clone();
                    Some(
                        models
                            .into_iter()
                            .filter_map(|model| serde_json::from_value::<ClaudeModel>(model).ok())
                            .collect(),
                    )
                })
                .unwrap_or_default()
        }
        .boxed()
        .shared();
        Ok(())
    }

    /// Drives a spawned process: reads its output and writes its input.
    fn attach(
        process: Process,
        mut output: mpsc::UnboundedReceiver<ProcessOutput>,
        session_id: Option<String>,
        tools: Option<ToolServer>,
        own_model: Option<String>,
    ) -> Self {
        let (writes, mut queue) =
            mpsc::unbounded_channel::<(String, oneshot::Sender<Result<(), String>>)>();
        let inner = Arc::new(Inner {
            process: process.clone(),
            own_model,
            tools: Mutex::new(tools),
            state: Mutex::new(State {
                session_id,
                conversation: None,
                pending: 0,
                unread: 0,
                state_changes: 0,
                began: false,
                waiters: Vec::new(),
                buffer: Lines::default(),
                stderr: String::new(),
                last_error: None,
                alive: true,
                reports_state: false,
                requests: 0,
                calls: HashMap::new(),
                listed: futures::future::ready(Vec::new()).boxed().shared(),
                reported: None,
                picked_model: None,
                effort: None,
                own_effort: None,
                reading_effort: None,
                receipts: HashMap::new(),
                active: HashMap::new(),
            }),
            writes,
            exited: watch::Sender::new(None),
        });
        tokio::spawn(async move {
            while let Some((line, done)) = queue.recv().await {
                let _ = done.send(process.write(line).await);
            }
        });
        let claude = Self { inner };
        let reader = claude.clone();
        tokio::spawn(async move {
            let mut code = None;
            while let Some(item) = output.recv().await {
                match item {
                    ProcessOutput::Stdout(data) => reader.read(&data),
                    ProcessOutput::Stderr(data) => reader.read_error(&data),
                    ProcessOutput::Exit(exit) => {
                        code = exit;
                        break;
                    }
                }
            }
            reader.on_exit(code);
        });
        claude
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
    fn tools(&self) -> MutexGuard<'_, Option<ToolServer>> {
        self.inner
            .tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn running(&self) -> bool {
        self.state().alive
    }
    pub fn busy(&self) -> bool {
        self.state().pending > 0
    }
    pub fn started(&self) -> bool {
        self.state().began
    }
    pub fn session_id(&self) -> Option<String> {
        self.state().session_id.clone()
    }
    pub fn set_conversation(&self, conversation: &str) {
        self.state().conversation = Some(conversation.into());
    }

    /// Resolves with the exit code once the process is gone.
    pub async fn exited(&self) -> Option<i32> {
        let mut exited = self.inner.exited.subscribe();
        match exited.wait_for(Option::is_some).await {
            Ok(code) => code.flatten(),
            Err(_) => None,
        }
    }

    /// The models this session can switch to, without sending a turn.
    pub fn models(&self) -> impl std::future::Future<Output = Vec<ClaudeModel>> + Send + 'static {
        self.state().listed.clone()
    }

    /// The effort a turn without a pick runs at, when Claude reports
    /// it.
    pub async fn read_effort(&self) -> Option<String> {
        self.read_own_effort().await;
        self.state().own_effort.clone()
    }

    /// Settings resolve the default model without sending a paid user turn.
    pub async fn read_model(&self) -> Option<String> {
        {
            let state = self.state();
            // Settings report a picked model, which is not the session's
            // own.
            if state.reported.is_some() || state.picked_model.is_some() {
                return state.reported.clone();
            }
        }
        let answer = self.call(json!({ "subtype": "get_settings" })).await;
        let mut state = self.state();
        if let Some(model) = answer
            .pointer("/response/applied/model")
            .and_then(Value::as_str)
        {
            state.reported = Some(model.into());
        }
        state.reported.clone()
    }

    /// Sends one user message. Resolves when it, and anything sent after
    /// it, has been answered and the session is idle again. A new turn
    /// runs with the picked model and effort, or the session's own without
    /// them; a message folded into a running turn changes only what it
    /// asks for.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn send(
        &self,
        text: &str,
        pick: TurnPick,
    ) -> impl std::future::Future<Output = Settled> + Send + 'static {
        self.send_with(text, pick, None)
    }

    /// [`Self::send`], with the mention's work row: Claude's echo of this
    /// exact message moves its thread's activity to it, and its steps show
    /// there.
    pub fn send_with(
        &self,
        text: &str,
        pick: TurnPick,
        receipt: Option<Receipt>,
    ) -> impl std::future::Future<Output = Settled> + Send + 'static {
        let (sender, receiver) = oneshot::channel();
        let uuid = uuid::Uuid::new_v4().to_string();
        let (alive, busy, picked_model, effort) = {
            let state = self.state();
            (
                state.alive,
                state.pending > 0,
                state.picked_model.clone(),
                state.effort.clone(),
            )
        };
        if alive {
            // The model first: effort applies to whichever model runs.
            self.set_model(if pick.model.is_some() || !busy {
                pick.model
            } else {
                picked_model
            });
            self.set_effort(if pick.effort.is_some() || !busy {
                pick.effort
            } else {
                effort
            });
        }
        let alive = {
            let mut state = self.state();
            if state.alive {
                state.pending += 1;
                state.unread += 1;
                state.waiters.push(sender);
                if let Some(receipt) = receipt {
                    state.receipts.insert(uuid.clone(), receipt);
                }
            }
            state.alive
        };
        let write = alive.then(|| {
            self.write(json!({
                "type": "user",
                "uuid": uuid,
                "message": { "role": "user", "content": text },
                "parent_tool_use_id": null,
                "session_id": "",
            }))
        });
        let claude = self.clone();
        async move {
            let Some(write) = write else {
                return Settled::failed("Claude is not running");
            };
            if let Err(error) = write.await {
                claude.settle(Settled::failed(format!(
                    "Claude did not take the message: {error}"
                )));
            }
            receiver
                .await
                .unwrap_or_else(|_| Settled::failed("Claude was stopped"))
        }
    }

    pub async fn kill(&self) {
        let _ = self.inner.process.kill().await;
    }

    fn write(
        &self,
        message: Value,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'static {
        let (done, result) = oneshot::channel();
        let queued = self.inner.writes.send((format!("{message}\n"), done));
        async move {
            queued.map_err(|_| "Claude is not running".to_string())?;
            result
                .await
                .unwrap_or_else(|_| Err("Claude is not running".into()))
        }
    }

    /// Writes a control request: the first future settles with the write,
    /// the second with Claude's response.
    fn request(
        &self,
        request: Value,
    ) -> (
        impl std::future::Future<Output = Result<(), String>> + Send + 'static,
        oneshot::Receiver<Value>,
    ) {
        let (answer, answered) = oneshot::channel();
        let id = {
            let mut state = self.state();
            state.requests += 1;
            let id = format!("buzz-{}", state.requests);
            if state.alive {
                state.calls.insert(id.clone(), answer);
            }
            id
        };
        let written = self.write(json!({
            "type": "control_request",
            "request_id": id,
            "request": request,
        }));
        (written, answered)
    }

    /// Sends a control request and resolves with Claude's response.
    fn call(&self, request: Value) -> impl std::future::Future<Output = Value> + Send + 'static {
        let (written, answer) = self.request(request);
        async move {
            let failed = |error: String| json!({ "subtype": "error", "error": error });
            if let Err(error) = written.await {
                return failed(error);
            }
            answer
                .await
                .unwrap_or_else(|_| failed("Claude exited".into()))
        }
    }

    /// Switches model without restarting, as the SDK's `setModel` does.
    /// Back to the session's own is the model it was launched with, or the
    /// default. A new model misses the prompt cache, which the owner
    /// accepts by picking.
    fn set_model(&self, model: Option<String>) {
        {
            let mut state = self.state();
            if state.picked_model == model {
                return;
            }
            state.picked_model = model.clone();
        }
        let mut request = json!({ "subtype": "set_model" });
        if let Some(next) = model.or_else(|| self.inner.own_model.clone()) {
            request["model"] = json!(next);
        }
        let answer = self.call(request);
        tokio::spawn(async move {
            let answer = answer.await;
            if answer["subtype"] != "success" {
                tracing::warn!(error = %answer["error"], "Claude did not change its model");
            }
        });
    }

    /// Changes effort without restarting, which keeps the prompt cache:
    /// Claude sends effort per message. Clearing the flag setting
    /// would not restore the session's own level, so that is read before
    /// the first change and set back explicitly. Written ahead of the
    /// message without waiting, as Claude takes its input in order, so
    /// a turn never waits on it.
    fn set_effort(&self, effort: Option<String>) {
        if self.state().effort == effort {
            return;
        }
        drop(self.read_own_effort());
        let level = {
            let mut state = self.state();
            let Some(level) = effort.clone().or_else(|| state.own_effort.clone()) else {
                tracing::warn!("Claude's own effort is unknown; keeping the last one");
                return;
            };
            state.effort = effort;
            level
        };
        let answer = self.call(json!({
            "subtype": "apply_flag_settings",
            "settings": { "effortLevel": level },
        }));
        tokio::spawn(async move {
            let answer = answer.await;
            if answer["subtype"] != "success" {
                tracing::warn!(error = %answer["error"], "Claude did not change its effort");
            }
        });
    }

    /// Read once, ahead of the first change, so it is the session's own.
    fn read_own_effort(&self) -> Shared<BoxFuture<'static, ()>> {
        if let Some(reading) = self.state().reading_effort.clone() {
            return reading;
        }
        let answer = self.call(json!({ "subtype": "get_settings" }));
        let claude = self.clone();
        let reading = async move {
            let answer = answer.await;
            if let Some(effort) = answer
                .pointer("/response/applied/effort")
                .and_then(Value::as_str)
            {
                claude.state().own_effort = Some(effort.into());
            }
        }
        .boxed()
        .shared();
        // Started now, so its answer is read even before anyone waits.
        tokio::spawn(reading.clone());
        self.state().reading_effort = Some(reading.clone());
        reading
    }

    fn settle(&self, settled: Settled) {
        let waiters = {
            let mut state = self.state();
            state.pending = 0;
            state.unread = 0;
            state.last_error = None;
            state.receipts.clear();
            state.active.clear();
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            let _ = waiter.send(settled.clone());
        }
    }

    fn on_exit(&self, code: Option<i32>) {
        let calls: Vec<oneshot::Sender<Value>> =
            self.state().calls.drain().map(|(_, call)| call).collect();
        for call in calls {
            let _ = call.send(json!({ "subtype": "error", "error": "Claude exited" }));
        }
        let error = {
            let mut state = self.state();
            state.alive = false;
            state.last_error.clone().unwrap_or_else(|| {
                let last = state.stderr.trim().lines().last().unwrap_or("").to_string();
                if last.is_empty() {
                    format!(
                        "Claude exited ({})",
                        code.map_or("signal".into(), |code| code.to_string())
                    )
                } else {
                    last
                }
            })
        };
        self.settle(Settled::failed(error));
        self.inner.exited.send_replace(Some(code));
    }

    fn read_error(&self, data: &str) {
        let mut state = self.state();
        state.stderr.push_str(data);
        if state.stderr.len() > STDERR_LIMIT {
            let mut cut = state.stderr.len() - STDERR_LIMIT;
            while !state.stderr.is_char_boundary(cut) {
                cut += 1;
            }
            state.stderr.drain(..cut);
        }
    }

    fn read(&self, data: &str) {
        let (lines, oversized) = self.state().buffer.push(data, FRAME_LIMIT);
        for line in lines {
            if let Ok(message) = serde_json::from_str::<Value>(line.trim()) {
                self.handle(message);
            }
        }
        // Its output can't be followed any more, so it stops.
        if oversized {
            self.state().last_error = Some("Claude output frame is too large".into());
            let claude = self.clone();
            tokio::spawn(async move { claude.kill().await });
        }
    }

    fn handle(&self, message: Value) {
        let kind = message.get("type").and_then(Value::as_str);
        let subtype = message.get("subtype").and_then(Value::as_str);
        {
            let mut state = self.state();
            if let Some(id) = message
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                state.session_id = Some(id.into());
            }
            if kind == Some("system") && subtype == Some("init") {
                state.began = true;
                // A picked model is not the session's own.
                if let Some(model) = message.get("model").and_then(Value::as_str)
                    && state.picked_model.is_none()
                {
                    state.reported = Some(model.into());
                }
            }
        }
        if kind == Some("control_request") {
            self.answer(message);
            return;
        }
        if kind == Some("control_response") {
            let response = message
                .get("response")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let id = match response.get("request_id") {
                Some(Value::String(id)) => id.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let call = self.state().calls.remove(&id);
            if let Some(call) = call {
                let _ = call.send(response);
            }
            return;
        }
        if kind == Some("user") && message.get("isReplay") == Some(&Value::Bool(true)) {
            // Acknowledgements match the uuid sent, never prompt text or
            // tool results. Only the latest one in a thread gets steps.
            let (taken, replaced) = {
                let mut state = self.state();
                state.unread = state.unread.saturating_sub(1);
                let receipt = message
                    .get("uuid")
                    .and_then(Value::as_str)
                    .and_then(|uuid| state.receipts.remove(uuid));
                match receipt {
                    Some(receipt) => {
                        let replaced = state.active.insert(receipt.scope.clone(), receipt.clone());
                        (Some(receipt), replaced)
                    }
                    None => (None, None),
                }
            };
            if let Some(replaced) = replaced {
                replaced.work.supersede();
            }
            if let Some(taken) = taken {
                taken.work.started();
            }
            return;
        }
        if kind == Some("assistant") {
            if let Some((tool, target)) = step_label(&message) {
                let active: Vec<Receipt> = self.state().active.values().cloned().collect();
                for receipt in active {
                    receipt.work.step(&tool, target.as_deref());
                }
            }
            return;
        }
        if kind == Some("system") && subtype == Some("session_state_changed") {
            let (idle, unread, change) = {
                let mut state = self.state();
                state.reports_state = true;
                state.state_changes += 1;
                let idle = message.get("state").and_then(Value::as_str) == Some("idle")
                    && state.pending > 0;
                (idle, state.unread, state.state_changes)
            };
            if idle && unread == 0 {
                self.done();
            } else if idle {
                // A message written as the turn ended is still to be read:
                // Claude goes running again for it. Should it stay idle
                // instead, the messages are settled anyway, never left
                // waiting.
                let claude = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(UNREAD_GRACE).await;
                    let still_idle = {
                        let state = claude.state();
                        state.state_changes == change && state.pending > 0
                    };
                    if still_idle {
                        claude.done();
                    }
                });
            }
            return;
        }
        if kind != Some("result") {
            return;
        }
        let done = {
            let mut state = self.state();
            // An interrupt is not a failure to report.
            if message.get("is_error").is_some_and(truthy)
                && message.get("terminal_reason").and_then(Value::as_str)
                    != Some("aborted_streaming")
            {
                state.last_error = Some(
                    message
                        .get("result")
                        .and_then(Value::as_str)
                        .filter(|result| !result.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!("Claude stopped ({})", subtype.unwrap_or("undefined"))
                        }),
                );
            }
            !state.reports_state && state.pending > 0
        };
        if done {
            self.done();
        }
    }

    fn done(&self) {
        let error = self.state().last_error.clone();
        self.settle(match error {
            Some(error) => Settled::failed(error),
            None => Settled::ok(),
        });
    }

    /// Answers what Claude asks its host: calls to the Buzz tools, the
    /// hook after `send`, and, since permissions are bypassed, anything
    /// else only so an unexpected request does not stall the turn.
    fn answer(&self, message: Value) {
        let id = message.get("request_id").cloned().unwrap_or(Value::Null);
        let request = message.get("request").cloned().unwrap_or_else(|| json!({}));
        let subtype = request.get("subtype").and_then(Value::as_str);
        let success = |response: Value| json!({ "subtype": "success", "request_id": id, "response": response });
        let tools = self.tools().clone();
        let response = match (subtype, tools) {
            (Some("mcp_message"), Some(tools)) => {
                let conversation = self.state().conversation.clone();
                let mcp = request.get("message").cloned().unwrap_or(Value::Null);
                let claude = self.clone();
                tokio::spawn(async move {
                    let call_id = mcp.get("id").cloned().unwrap_or(json!(0));
                    let call_id = if call_id.is_null() { json!(0) } else { call_id };
                    let answer = tools(conversation, mcp).await.unwrap_or_else(|error| {
                        json!({
                            "jsonrpc": "2.0",
                            "id": call_id,
                            "error": { "code": -32603, "message": error },
                        })
                    });
                    let response = json!({
                        "subtype": "success",
                        "request_id": id,
                        "response": { "mcp_response": answer },
                    });
                    let _ = claude
                        .write(json!({ "type": "control_response", "response": response }))
                        .await;
                });
                return;
            }
            (Some("can_use_tool"), _) => success(json!({
                "behavior": "allow",
                "updatedInput": request.get("input").cloned().unwrap_or_else(|| json!({})),
            })),
            (Some("hook_callback"), _) => success(if self.is_final(request.get("input")) {
                json!({ "continue": false })
            } else {
                json!({})
            }),
            (other, _) => json!({
                "subtype": "error",
                "request_id": id,
                "error": format!("Buzz does not handle {}", other.unwrap_or("undefined")),
            }),
        };
        let write = self.write(json!({ "type": "control_response", "response": response }));
        tokio::spawn(async move {
            let _ = write.await;
        });
    }

    /// A `send` marked final succeeded (a failed call runs no PostToolUse
    /// hook), so the turn can end without a closing model call. Not when
    /// another message came in meanwhile: that one still needs an answer.
    fn is_final(&self, input: Option<&Value>) -> bool {
        let input = input.cloned().unwrap_or(Value::Null);
        input.get("tool_name").and_then(Value::as_str) == Some(FINAL_TOOL)
            && input
                .get("tool_input")
                .and_then(|tool_input| tool_input.get("final"))
                == Some(&Value::Bool(true))
            && self.state().pending == 1
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;

/// The last block of an assistant message as the tool it names and what
/// it acts on (`stepLabel`).
pub fn step_label(message: &Value) -> Option<(String, Option<String>)> {
    let block = message.get("message")?.get("content")?.as_array()?.last()?;
    match block.get("type").and_then(Value::as_str) {
        Some("tool_use") => {
            let tool = block.get("name")?.as_str()?.rsplit("__").next()?;
            if tool.is_empty() {
                return None;
            }
            let input = block.get("input");
            let target = [
                "description",
                "file_path",
                "notebook_path",
                "pattern",
                "url",
                "query",
                "command",
            ]
            .iter()
            .find_map(|key| {
                input
                    .and_then(|input| input.get(*key))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
            })
            .map(short_target);
            Some((tool.to_string(), target))
        }
        Some("thinking") => Some(("Thinking".into(), None)),
        Some("text") => Some(("Writing reply".into(), None)),
        _ => None,
    }
}

/// File paths show their name; anything else passes through.
fn short_target(value: &str) -> String {
    if value.starts_with('/') {
        value.rsplit('/').next().unwrap_or(value).to_string()
    } else {
        value.to_string()
    }
}
