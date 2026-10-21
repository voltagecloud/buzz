//! A stand-in for the `claude` binary's stream-json protocol, for tests
//! (buzz-app's `claude-testing.ts`).

use crate::agent_plugins::process::{
    Process, ProcessControl, Spawn, SpawnOptions, Spawned, Spawner,
};
use buzz_host::api::ProcessOutput;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::mpsc;

pub struct FakeState {
    pub id: String,
    pub options: SpawnOptions,
    /// Every JSON message written to its stdin.
    pub received: Vec<Value>,
    /// User messages it has taken, in order.
    pub prompts: Vec<String>,
    /// The session's effort, which `apply_flag_settings` changes.
    pub effort: String,
    /// The effort each user message was taken at.
    pub efforts: Vec<String>,
    /// The session's model, which `set_model` changes.
    pub model: String,
    /// The model each user message was taken on.
    pub models: Vec<String>,
    /// When set, a turn stays open until `finish()`.
    pub hold: bool,
    pub killed: bool,
    /// When false, a kill is only a request; the test calls `exit()`.
    pub exit_on_kill: bool,
    session: String,
    resume: Option<String>,
    open: usize,
    started: bool,
    alive: bool,
    output: mpsc::UnboundedSender<ProcessOutput>,
}

impl FakeState {
    /// Claude reports running and idle only when asked to.
    fn reports_state(&self) -> bool {
        self.options
            .env
            .get("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS")
            == Some(&Some("1".into()))
    }
}

/// The models the fake lists, as Claude's `initialize` answer does.
pub fn listed_models() -> Value {
    json!([
        { "value": "default", "resolvedModel": "claude-opus-5-5" },
        {
            "value": "opus",
            "resolvedModel": "claude-opus-5-5",
            "supportsEffort": true,
            "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"],
        },
        {
            "value": "haiku",
            "resolvedModel": "claude-haiku-5-5",
            "supportsEffort": true,
            "supportedEffortLevels": ["low", "high"],
        },
        { "value": "legacy", "resolvedModel": "claude-legacy-1", "supportsEffort": false },
    ])
}

fn resolved(model: &str) -> String {
    listed_models()
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["value"] == model)
        .and_then(|item| item["resolvedModel"].as_str())
        .unwrap_or(model)
        .to_string()
}

/// One fake process. Cheap to clone.
#[derive(Clone)]
pub struct FakeClaude(Arc<Mutex<FakeState>>, Arc<Mutex<HashSet<String>>>);

impl FakeClaude {
    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.0.lock().unwrap()
    }
    pub fn args(&self) -> Vec<String> {
        self.state().options.args.clone()
    }
    pub fn prompts(&self) -> Vec<String> {
        self.state().prompts.clone()
    }
    pub fn efforts(&self) -> Vec<String> {
        self.state().efforts.clone()
    }
    pub fn models(&self) -> Vec<String> {
        self.state().models.clone()
    }
    /// The control requests of `subtype` it was sent, in order.
    pub fn requests(&self, subtype: &str) -> Vec<Value> {
        self.state()
            .received
            .iter()
            .filter(|message| {
                message["type"] == "control_request" && message["request"]["subtype"] == subtype
            })
            .map(|message| message["request"].clone())
            .collect()
    }
    fn flag(&self, name: &str) -> Option<String> {
        let args = self.args();
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1).cloned())
    }
    pub fn killed(&self) -> bool {
        self.state().killed
    }
    pub fn set_hold(&self, hold: bool) {
        self.state().hold = hold;
    }
    /// The control responses it was sent, in order.
    pub fn responses(&self) -> Vec<Value> {
        self.state()
            .received
            .iter()
            .filter(|message| message["type"] == "control_response")
            .map(|message| message["response"].clone())
            .collect()
    }

    pub fn emit(&self, message: Value) {
        let state = self.state();
        if state.alive {
            let _ = state
                .output
                .send(ProcessOutput::Stdout(format!("{message}\n")));
        }
    }

    pub fn finish(&self, error: Option<&str>) {
        let (session, idle) = {
            let mut state = self.state();
            if state.open == 0 {
                return;
            }
            state.open = 0;
            (state.session.clone(), state.reports_state())
        };
        let mut result = json!({
            "type": "result",
            "subtype": if error.is_some() { "error_during_execution" } else { "success" },
            "is_error": error.is_some(),
            "session_id": session,
        });
        if let Some(error) = error {
            result["result"] = json!(error);
        }
        self.emit(result);
        // Claude reports idle only when asked to.
        if idle {
            self.emit(json!({
                "type": "system", "subtype": "session_state_changed",
                "state": "idle", "session_id": session,
            }));
        }
    }

    pub fn exit(&self, code: Option<i32>, stderr: Option<&str>) {
        let mut state = self.state();
        if !state.alive {
            return;
        }
        if let Some(stderr) = stderr {
            let _ = state.output.send(ProcessOutput::Stderr(stderr.into()));
        }
        state.alive = false;
        let _ = state.output.send(ProcessOutput::Exit(code));
    }

    fn take(&self, message: Value) {
        self.state().received.push(message.clone());
        if message["type"] == "control_request" {
            let request = &message["request"];
            let subtype = request["subtype"].as_str().unwrap_or_default();
            if let (Some("apply_flag_settings"), Some(level)) =
                (Some(subtype), request["settings"]["effortLevel"].as_str())
            {
                self.state().effort = level.into();
            }
            if subtype == "set_model" {
                let model = request["model"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| self.flag("--model"))
                    .unwrap_or_else(|| "default".into());
                self.state().model = model;
            }
            let mut response = json!({ "subtype": "success", "request_id": message["request_id"] });
            if subtype == "get_settings" {
                let state = self.state();
                response["response"] = json!({
                    "applied": { "effort": state.effort, "model": resolved(&state.model) },
                });
            }
            if subtype == "initialize" {
                response["response"] = json!({ "models": listed_models() });
            }
            self.emit(json!({ "type": "control_response", "response": response }));
            return;
        }
        if message["type"] != "user" {
            return;
        }
        let resume = self.state().resume.clone();
        if let Some(resume) = resume
            && !self.1.lock().unwrap().contains(&resume)
        {
            self.exit(
                Some(1),
                Some(&format!(
                    "No conversation found with session ID: {resume}\n"
                )),
            );
            return;
        }
        let content = message["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let (session, first, hold, starts, model) = {
            let mut state = self.state();
            state.prompts.push(content.clone());
            let (effort, model) = (state.effort.clone(), state.model.clone());
            state.efforts.push(effort);
            state.models.push(model.clone());
            let first = !state.started;
            state.started = true;
            let starts = state.open == 0 && state.reports_state();
            state.open += 1;
            (state.session.clone(), first, state.hold, starts, model)
        };
        if starts {
            self.emit(json!({
                "type": "system", "subtype": "session_state_changed",
                "state": "running", "session_id": session,
            }));
        }
        if first {
            self.1.lock().unwrap().insert(session.clone());
            self.emit(json!({
                "type": "system", "subtype": "init", "session_id": session,
                "model": resolved(&model),
            }));
        }
        if self
            .args()
            .iter()
            .any(|arg| arg == "--replay-user-messages")
        {
            self.emit(json!({
                "type": "user", "isReplay": true, "session_id": session,
                "uuid": message["uuid"],
                "message": { "role": "user", "content": content },
            }));
        }
        self.emit(json!({
            "type": "assistant",
            "session_id": session,
            "message": { "content": [{ "type": "text", "text": format!("ok: {content}") }] },
        }));
        if !hold {
            self.finish(None);
        }
    }
}

impl ProcessControl for FakeClaude {
    fn write(&self, data: String, _close: bool) -> BoxFuture<'static, Result<(), String>> {
        if !self.state().alive {
            return Box::pin(async { Err("Process has exited".into()) });
        }
        for line in data.split('\n').filter(|line| !line.is_empty()) {
            self.take(serde_json::from_str(line).unwrap());
        }
        Box::pin(async { Ok(()) })
    }
    fn kill(&self) -> BoxFuture<'static, Result<(), String>> {
        self.state().killed = true;
        if self.state().exit_on_kill {
            self.exit(None, None);
        }
        Box::pin(async { Ok(()) })
    }
}

/// A spawn whose `claude` processes answer like Claude. Sessions named
/// in `known` (or created by a process) can be resumed; others fail to
/// load.
#[derive(Clone, Default)]
pub struct FakeSpawn {
    pub processes: Arc<Mutex<Vec<FakeClaude>>>,
    pub known: Arc<Mutex<HashSet<String>>>,
    /// Every new process holds its turns open until finished.
    pub hold: bool,
}

impl FakeSpawn {
    pub fn spawner(&self) -> Spawner {
        Arc::new(self.clone())
    }
    pub fn all(&self) -> Vec<FakeClaude> {
        self.processes.lock().unwrap().clone()
    }
    pub fn claudes(&self) -> Vec<FakeClaude> {
        self.all()
            .into_iter()
            .filter(|process| process.state().id == "claude")
            .collect()
    }
    pub fn live(&self) -> Vec<FakeClaude> {
        self.claudes()
            .into_iter()
            .filter(|process| !process.killed())
            .collect()
    }
}

impl Spawn for FakeSpawn {
    fn spawn(
        &self,
        id: &str,
        options: SpawnOptions,
    ) -> BoxFuture<'static, Result<Spawned, String>> {
        let flag = |name: &str| {
            options
                .args
                .iter()
                .position(|arg| arg == name)
                .and_then(|index| options.args.get(index + 1).cloned())
        };
        let resume = flag("--resume");
        let session = resume
            .clone()
            .or_else(|| flag("--session-id"))
            .unwrap_or_else(|| "fresh".into());
        let model = flag("--model").unwrap_or_else(|| "default".into());
        let (output, receiver) = mpsc::unbounded_channel();
        let fake = FakeClaude(
            Arc::new(Mutex::new(FakeState {
                id: id.into(),
                options,
                received: Vec::new(),
                prompts: Vec::new(),
                effort: "high".into(),
                efforts: Vec::new(),
                model,
                models: Vec::new(),
                hold: self.hold,
                killed: false,
                exit_on_kill: true,
                session,
                resume,
                open: 0,
                started: false,
                alive: true,
                output,
            })),
            self.known.clone(),
        );
        self.processes.lock().unwrap().push(fake.clone());
        if id != "claude" {
            fake.exit(Some(0), None);
        }
        Box::pin(async move {
            Ok(Spawned {
                process: Process(Arc::new(fake)),
                output: receiver,
            })
        })
    }

    /// Clones are one host; each new fake is another.
    fn host(&self) -> usize {
        Arc::as_ptr(&self.processes) as *const () as usize
    }
}

/// Lets spawned tasks run.
pub async fn flush() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// Waits until `ready` holds, for at most five seconds.
pub async fn until(mut ready: impl FnMut() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting");
}
