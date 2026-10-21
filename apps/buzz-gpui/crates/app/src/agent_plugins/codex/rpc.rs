//! One `codex app-server` and its JSON-RPC over the process's stdio, a port
//! of buzz-app's `rpc.ts`. The server runs on the host that holds the
//! agent, as a declared process; requests and their answers pair by id,
//! the server's own requests for Buzz tools are answered here, and its
//! notifications go to whoever listens.

use super::config::Model;
use crate::agent_plugins::process::{Lines, Process, SpawnOptions, Spawner};
use crate::buzz_mcp::tools::Reply;
use buzz_host::api::ProcessOutput;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

/// The longest a request waits for its answer.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a closing server has to stop on its own before it is killed.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// The largest line the server may write.
const FRAME_LIMIT: usize = 4 * 1024 * 1024;
const STDERR_LIMIT: usize = 4000;

/// A notification from the server, or `connection/closed` once it is gone.
pub type Listener = Arc<dyn Fn(&Value) + Send + Sync>;

/// One `item/tool/call`: the server asks for a dynamic tool.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolCall {
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub call_id: String,
    pub namespace: Option<String>,
    pub tool: String,
    pub arguments: Value,
}

impl ToolCall {
    fn parse(params: &Value) -> Self {
        let text = |name: &str| params.get(name).and_then(Value::as_str).map(str::to_string);
        Self {
            thread_id: text("threadId"),
            turn_id: text("turnId"),
            call_id: text("callId").unwrap_or_default(),
            namespace: text("namespace"),
            tool: text("tool").unwrap_or_default(),
            arguments: params.get("arguments").cloned().unwrap_or(Value::Null),
        }
    }
}

/// Answers a tool call: its text or image, or an error for Codex to
/// correct.
pub type ToolHandler =
    Arc<dyn Fn(ToolCall) -> BoxFuture<'static, Result<Reply, String>> + Send + Sync>;

type Answer = oneshot::Sender<Result<Value, String>>;

#[derive(Default)]
struct State {
    process: Option<Process>,
    failure: Option<String>,
    pending: HashMap<u64, Answer>,
    listeners: Vec<(u64, Listener)>,
    closing: Option<Shared<BoxFuture<'static, ()>>>,
    buffer: Lines,
    stderr: String,
}

struct Inner {
    tool: Option<ToolHandler>,
    state: Mutex<State>,
    next: AtomicU64,
    /// Whether the process has exited.
    exited: watch::Sender<bool>,
}

/// One request owner for the app-server connection. Cheap to clone.
#[derive(Clone)]
pub struct AppServer {
    inner: Arc<Inner>,
}

fn error_text(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Codex request failed")
        .to_string()
}

impl AppServer {
    /// A connection that answers Buzz tool calls with `tool`; without it,
    /// tool calls are refused like any interactive request.
    pub fn new(tool: Option<ToolHandler>) -> Self {
        Self {
            inner: Arc::new(Inner {
                tool,
                state: Mutex::default(),
                next: AtomicU64::new(0),
                exited: watch::Sender::new(false),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts the server, in `cwd` if given, and introduces this client.
    /// Must run inside tokio.
    pub async fn open(&self, spawn: &Spawner, cwd: Option<String>) -> Result<(), String> {
        let spawned = spawn
            .spawn(
                "app-server",
                SpawnOptions {
                    cwd,
                    ..Default::default()
                },
            )
            .await?;
        self.state().process = Some(spawned.process);
        tokio::spawn(self.clone().pump(spawned.output));
        // Closed while it started: it stops at once.
        let failure = self.state().failure.clone();
        if let Some(failure) = failure {
            self.close().await;
            return Err(failure);
        }
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "buzz_codex_plugin",
                    "title": "Buzz Codex",
                    "version": "0.1.0",
                },
                "capabilities": {
                    "experimentalApi": true,
                    "optOutNotificationMethods": [
                        "item/agentMessage/delta",
                        "item/reasoning/summaryTextDelta",
                        "item/reasoning/textDelta",
                        "item/plan/delta",
                        "item/commandExecution/outputDelta",
                        "item/fileChange/outputDelta",
                    ],
                },
            }),
        )
        .await?;
        self.send(json!({ "method": "initialized" })).await?;
        Ok(())
    }

    /// Reads the process's output until it exits.
    async fn pump(self, mut output: mpsc::UnboundedReceiver<ProcessOutput>) {
        let mut code = None;
        while let Some(item) = output.recv().await {
            match item {
                ProcessOutput::Stdout(text) => self.read(&text),
                ProcessOutput::Stderr(text) => {
                    let mut state = self.state();
                    state.stderr.push_str(&text);
                    let excess = state.stderr.len().saturating_sub(STDERR_LIMIT);
                    if excess > 0 {
                        let cut = (excess..=state.stderr.len())
                            .find(|index| state.stderr.is_char_boundary(*index))
                            .unwrap_or(state.stderr.len());
                        state.stderr.drain(..cut);
                    }
                }
                ProcessOutput::Exit(exit) => {
                    code = exit;
                    break;
                }
            }
        }
        let stderr = self.state().stderr.trim().to_string();
        self.fail(if stderr.is_empty() {
            format!(
                "Codex exited ({})",
                code.map_or("signal".to_string(), |code| code.to_string())
            )
        } else {
            stderr
        });
        self.inner.exited.send_replace(true);
    }

    fn read(&self, data: &str) {
        let (lines, oversized) = self.state().buffer.push(data, FRAME_LIMIT);
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(message) => self.handle(message),
                Err(error) => return self.broken(error.to_string()),
            }
        }
        if oversized {
            self.broken("Codex output frame is too large".into());
        }
    }

    /// Unreadable output ends the connection.
    fn broken(&self, error: String) {
        self.fail(error);
        drop(self.close());
    }

    fn handle(&self, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").filter(|id| !id.is_null());
        let waiter = match (id.and_then(Value::as_u64), method) {
            (Some(id), None) => self.state().pending.remove(&id),
            _ => None,
        };
        if let Some(waiter) = waiter {
            let _ = waiter.send(match message.get("error") {
                Some(error) if !error.is_null() => Err(error_text(error)),
                _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            });
            return;
        }
        let (Some(id), Some(method)) = (id, method) else {
            return self.emit(&message);
        };
        let id = id.clone();
        if let (Some(tool), "item/tool/call") = (&self.inner.tool, method) {
            let call = ToolCall::parse(message.get("params").unwrap_or(&Value::Null));
            let (this, answer) = (self.clone(), tool(call));
            tokio::spawn(async move {
                let (success, item) = match answer.await {
                    Ok(Reply::Text(text)) => (true, json!({ "type": "inputText", "text": text })),
                    Ok(Reply::Image { data, mime }) => (
                        true,
                        json!({ "type": "inputImage", "imageUrl": format!("data:{mime};base64,{data}") }),
                    ),
                    Err(text) => (false, json!({ "type": "inputText", "text": text })),
                };
                let reply = json!({
                    "id": id,
                    "result": { "success": success, "contentItems": [item] },
                });
                if let Err(error) = this.send(reply).await {
                    this.fail(error);
                }
            });
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let refusal = json!({
                "id": id,
                "error": {
                    "code": -32601,
                    "message": "Buzz Codex runs with approvalPolicy=never; interactive requests are unsupported",
                },
            });
            if let Err(error) = this.send(refusal).await {
                this.fail(error);
            }
        });
    }

    fn emit(&self, message: &Value) {
        let listeners: Vec<Listener> = self
            .state()
            .listeners
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect();
        for listener in listeners {
            // One listener's bug must not strand the rest, or the exit.
            if std::panic::catch_unwind(AssertUnwindSafe(|| listener(message))).is_err() {
                tracing::error!("Codex notification handler failed");
            }
        }
    }

    /// Calls `listener` with each notification until [`Self::off`].
    pub fn on_message(&self, listener: Listener) -> u64 {
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.state().listeners.push((id, listener));
        id
    }

    pub fn off(&self, listener: u64) {
        self.state().listeners.retain(|(id, _)| *id != listener);
    }

    /// Ends the connection: requests waiting fail with `error`, and
    /// listeners hear `connection/closed`. Only the first failure counts.
    fn fail(&self, error: String) {
        let pending = {
            let mut state = self.state();
            if state.failure.is_some() {
                return;
            }
            state.failure = Some(error.clone());
            std::mem::take(&mut state.pending)
        };
        for (_, waiter) in pending {
            let _ = waiter.send(Err(error.clone()));
        }
        self.emit(&json!({
            "method": "connection/closed",
            "params": { "message": error },
        }));
    }

    pub async fn send(&self, message: Value) -> Result<(), String> {
        let process = {
            let state = self.state();
            if let Some(failure) = &state.failure {
                return Err(failure.clone());
            }
            state
                .process
                .clone()
                .ok_or("Codex is not connected".to_string())?
        };
        process.write(format!("{message}\n")).await
    }

    /// Asks the server and waits for its answer, at most 30 seconds.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let (id, answer) = {
            let mut state = self.state();
            if let Some(failure) = &state.failure {
                return Err(failure.clone());
            }
            let id = self.inner.next.fetch_add(1, Ordering::Relaxed) + 1;
            let (sender, answer) = oneshot::channel();
            state.pending.insert(id, sender);
            (id, answer)
        };
        let waited = tokio::time::timeout(REQUEST_TIMEOUT, async {
            self.send(json!({ "id": id, "method": method, "params": params }))
                .await?;
            answer
                .await
                .unwrap_or_else(|_| Err("Codex connection closed".into()))
        })
        .await;
        match waited {
            Ok(result) => {
                if result.is_err() {
                    self.state().pending.remove(&id);
                }
                result
            }
            Err(_) => {
                self.state().pending.remove(&id);
                Err(format!("Codex {method} timed out"))
            }
        }
    }

    /// Ends the connection and stops the server: closing its input lets
    /// Codex stop its own background terminals; it is killed if it has not
    /// exited 3 seconds later. Runs to the end even if not awaited. Must
    /// run inside tokio.
    pub fn close(&self) -> Shared<BoxFuture<'static, ()>> {
        self.fail("Codex connection closed".into());
        let mut state = self.state();
        if let Some(closing) = &state.closing {
            return closing.clone();
        }
        let Some(process) = state.process.clone() else {
            return futures::future::ready(()).boxed().shared();
        };
        let mut exited = self.inner.exited.subscribe();
        let task = tokio::spawn(async move {
            let killer = {
                let process = process.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(CLOSE_GRACE).await;
                    let _ = process.kill().await;
                })
            };
            if process.end().await.is_err() {
                let _ = process.kill().await;
            }
            let _ = exited.wait_for(|exited| *exited).await;
            killer.abort();
        });
        let closing = async move {
            let _ = task.await;
        }
        .boxed()
        .shared();
        state.closing = Some(closing.clone());
        closing
    }
}

/// Every model Codex offers, page by page.
pub async fn list_models(rpc: &AppServer) -> Result<Vec<Model>, String> {
    let mut models = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let page = rpc
            .request("model/list", json!({ "limit": 100, "cursor": cursor }))
            .await?;
        let data = page.get("data").cloned().unwrap_or_default();
        models.extend(
            serde_json::from_value::<Vec<Model>>(data)
                .map_err(|_| "Codex returned an invalid model list".to_string())?,
        );
        cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.is_null() {
            return Ok(models);
        }
    }
}

#[cfg(test)]
pub mod testing;
#[cfg(test)]
mod tests;
