//! A stand-in for `codex app-server` and the type's other declared
//! processes, for tests (buzz-app's runtime test fixture). Turns complete
//! only when a test says so; the runtime owns everything else.

use crate::agent_plugins::process::{
    Process, ProcessControl, Spawn, SpawnOptions, Spawned, Spawner,
};
use base64::Engine;
use buzz_host::api::ProcessOutput;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Something a test opens later.
pub type Gate = Shared<BoxFuture<'static, ()>>;

/// Answers a method's params with its result, or an error message.
pub type Answer = Arc<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>;

/// A gate and the call that opens it.
pub fn gate() -> (Gate, oneshot::Sender<()>) {
    let (open, opened) = oneshot::channel::<()>();
    (
        async move {
            let _ = opened.await;
        }
        .boxed()
        .shared(),
        open,
    )
}

/// The next spawn of a process waits: it signals `started`, then returns
/// only once `release` opens.
pub struct Held {
    pub started: oneshot::Sender<()>,
    pub release: Gate,
}

pub struct State {
    /// Every message written to an app-server, in order.
    pub sent: Vec<Value>,
    /// Every spawn: its process id and options.
    pub spawns: Vec<(String, SpawnOptions)>,
    /// Files read: the reader's directory and path.
    pub reads: Vec<(Option<String>, String)>,
    pub models: Value,
    pub account: Value,
    pub config: Value,
    /// Background terminals `list` reports until terminated.
    pub terminals: Vec<Value>,
    /// Terminating a terminal waits for this.
    pub termination: Option<Gate>,
    /// Methods answered with an error message instead.
    pub errors: HashMap<String, String>,
    /// Methods the test answers itself.
    pub answers: HashMap<String, Answer>,
    /// The exit code of `workspace`.
    pub workspace_exit: i32,
    /// App-servers whose input was closed, and processes killed.
    pub ended: usize,
    pub killed: usize,
    /// The next app-server spawn waits.
    pub hold_server: Option<Held>,
    /// The next reader waits to be returned, and never exits by itself.
    pub hold_read: Option<Held>,
    /// Readers killed.
    pub reads_killed: usize,
    /// Turns that completed: steering them fails, as in Codex.
    ended_turns: std::collections::HashSet<String>,
    thread: u32,
    turn: u32,
    /// The latest app-server's output.
    output: Option<mpsc::UnboundedSender<ProcessOutput>>,
}

/// The fake processes. Cheap to clone. The second lock is held while a
/// write is recorded and answered, so a message a test emits never lands
/// between a request the test has seen and that request's answer.
#[derive(Clone)]
pub struct FakeCodex(Arc<Mutex<State>>, Arc<Mutex<()>>);

impl Default for FakeCodex {
    fn default() -> Self {
        Self(
            Arc::new(Mutex::new(State {
                sent: Vec::new(),
                spawns: Vec::new(),
                reads: Vec::new(),
                models: json!([
                    {
                        "model": "test-model",
                        "displayName": "Test Model",
                        "isDefault": true,
                        "defaultReasoningEffort": "medium",
                        "supportedReasoningEfforts": [{ "reasoningEffort": "low", "description": "" }, { "reasoningEffort": "medium", "description": "" }, { "reasoningEffort": "high", "description": "" }],
                    },
                    {
                        "model": "deep-model",
                        "displayName": "",
                        "isDefault": false,
                        "defaultReasoningEffort": "high",
                        "supportedReasoningEfforts": [{ "reasoningEffort": "high", "description": "" }, { "reasoningEffort": "xhigh", "description": "" }],
                    },
                ]),
                account: json!({ "account": { "type": "chatgpt", "email": "test@example.com" } }),
                config: json!({
                    "config": {
                        "mcp_servers": {
                            "company.tools": { "command": "secret-tool", "enabled": true, "url": null },
                        },
                    },
                }),
                terminals: Vec::new(),
                termination: None,
                errors: HashMap::new(),
                answers: HashMap::new(),
                workspace_exit: 0,
                ended: 0,
                killed: 0,
                hold_server: None,
                hold_read: None,
                reads_killed: 0,
                ended_turns: Default::default(),
                thread: 0,
                turn: 0,
                output: None,
            })),
            Arc::default(),
        )
    }
}

impl FakeCodex {
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }
    pub fn spawner(&self) -> Spawner {
        Arc::new(self.clone())
    }
    /// The messages sent with `method`.
    pub fn calls(&self, method: &str) -> Vec<Value> {
        self.state()
            .sent
            .iter()
            .filter(|message| message["method"] == method)
            .cloned()
            .collect()
    }
    /// Every spawn of process `id`.
    pub fn spawned(&self, id: &str) -> Vec<SpawnOptions> {
        self.state()
            .spawns
            .iter()
            .filter(|(spawned, _)| spawned == id)
            .map(|(_, options)| options.clone())
            .collect()
    }
    /// The latest server writes `message`.
    pub fn emit(&self, message: Value) {
        let _writing = self.1.lock().unwrap();
        self.output(message);
    }
    fn output(&self, message: Value) {
        let mut state = self.state();
        if message["method"] == "turn/completed"
            && let Some(turn) = message["params"]["turn"]["id"].as_str()
        {
            state.ended_turns.insert(turn.into());
        }
        if let Some(output) = &state.output {
            let _ = output.send(ProcessOutput::Stdout(format!("{message}\n")));
        }
    }
    /// The latest server exits by itself.
    pub fn exit(&self, code: Option<i32>, stderr: &str) {
        if let Some(output) = self.state().output.take() {
            if !stderr.is_empty() {
                let _ = output.send(ProcessOutput::Stderr(stderr.into()));
            }
            let _ = output.send(ProcessOutput::Exit(code));
        }
    }

    /// Answers a request as it is written, so ids number in the order sent;
    /// a termination that must wait answers later, from the returned
    /// future.
    fn answer(&self, request: Value) -> Option<BoxFuture<'static, ()>> {
        let id = request.get("id").filter(|id| !id.is_null()).cloned()?;
        let method = request["method"].as_str()?;
        let params = &request["params"];
        let error = {
            let state = self.state();
            let ended = params["expectedTurnId"]
                .as_str()
                .is_some_and(|turn| state.ended_turns.contains(turn));
            if method == "turn/steer" && ended {
                Some("no active turn to steer".to_string())
            } else {
                state.errors.get(method).cloned()
            }
        };
        if let Some(error) = error {
            self.output(json!({ "id": id, "error": { "code": -32600, "message": error } }));
            return None;
        }
        let answer = self.state().answers.get(method).cloned();
        if let Some(answer) = answer {
            self.output(match answer(params) {
                Ok(result) => json!({ "id": id, "result": result }),
                Err(error) => json!({ "id": id, "error": { "code": -32600, "message": error } }),
            });
            return None;
        }
        let result = match method {
            "account/read" => self.state().account.clone(),
            "model/list" => json!({ "data": self.state().models.clone(), "nextCursor": null }),
            "config/read" => self.state().config.clone(),
            "thread/start" => {
                let thread = {
                    let mut state = self.state();
                    state.thread += 1;
                    state.thread
                };
                json!({ "thread": { "id": format!("thread-{thread}"), "cwd": params["cwd"] } })
            }
            "thread/resume" => {
                json!({ "thread": { "id": params["threadId"], "cwd": params["cwd"] } })
            }
            "turn/start" => {
                let turn = {
                    let mut state = self.state();
                    state.turn += 1;
                    state.turn
                };
                json!({ "turn": { "id": format!("turn-{turn}") } })
            }
            "turn/interrupt" => {
                self.output(json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": params["threadId"],
                        "turn": { "id": params["turnId"], "status": "interrupted" },
                    },
                }));
                json!({})
            }
            "thread/backgroundTerminals/list" => {
                json!({ "data": self.state().terminals.clone(), "nextCursor": null })
            }
            "thread/backgroundTerminals/terminate" => {
                let termination = self.state().termination.clone();
                if let Some(termination) = termination {
                    let fake = self.clone();
                    return Some(
                        async move {
                            termination.await;
                            fake.state().terminals.clear();
                            fake.emit(json!({ "id": id, "result": {} }));
                        }
                        .boxed(),
                    );
                }
                json!({})
            }
            _ => json!({}),
        };
        let started = (method == "turn/start").then(|| {
            json!({
                "method": "turn/started",
                "params": { "threadId": params["threadId"], "turn": result["turn"] },
            })
        });
        self.output(json!({ "id": id, "result": result }));
        // Codex announces each turn before anything happens in it.
        if let Some(started) = started {
            self.output(started);
        }
        None
    }
}

/// One app-server.
struct Server {
    fake: FakeCodex,
    output: mpsc::UnboundedSender<ProcessOutput>,
    alive: AtomicBool,
}

impl Server {
    fn exit(&self, code: Option<i32>) {
        if self.alive.swap(false, Ordering::SeqCst) {
            let _ = self.output.send(ProcessOutput::Exit(code));
        }
    }
}

impl ProcessControl for Server {
    fn write(&self, data: String, close: bool) -> BoxFuture<'static, Result<(), String>> {
        if !self.alive.load(Ordering::SeqCst) {
            return Box::pin(async { Err("Process has exited".into()) });
        }
        if close {
            self.fake.state().ended += 1;
            self.exit(Some(0));
            return Box::pin(async { Ok(()) });
        }
        let messages: Vec<Value> = data
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        // Recorded, numbered and answered together, so turn ids follow the
        // order sent and a test sees no request before its answer.
        let _writing = self.fake.1.lock().unwrap();
        self.fake.state().sent.extend(messages.iter().cloned());
        let later: Vec<_> = messages
            .into_iter()
            .filter_map(|message| self.fake.answer(message))
            .collect();
        Box::pin(async move {
            for answer in later {
                answer.await;
            }
            Ok(())
        })
    }
    fn kill(&self) -> BoxFuture<'static, Result<(), String>> {
        self.fake.state().killed += 1;
        self.exit(None);
        Box::pin(async { Ok(()) })
    }
}

/// A short process: `workspace` or `read`.
struct Short {
    fake: FakeCodex,
    output: mpsc::UnboundedSender<ProcessOutput>,
    alive: AtomicBool,
}

impl ProcessControl for Short {
    fn write(&self, _: String, _: bool) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn kill(&self) -> BoxFuture<'static, Result<(), String>> {
        if self.alive.swap(false, Ordering::SeqCst) {
            self.fake.state().reads_killed += 1;
            let _ = self.output.send(ProcessOutput::Exit(None));
        }
        Box::pin(async { Ok(()) })
    }
}

impl Spawn for FakeCodex {
    fn spawn(
        &self,
        id: &str,
        options: SpawnOptions,
    ) -> BoxFuture<'static, Result<Spawned, String>> {
        let fake = self.clone();
        let id = id.to_string();
        Box::pin(async move {
            fake.state().spawns.push((id.clone(), options.clone()));
            let (output, receiver) = mpsc::unbounded_channel();
            match id.as_str() {
                "app-server" => {
                    let server = Arc::new(Server {
                        fake: fake.clone(),
                        output: output.clone(),
                        alive: AtomicBool::new(true),
                    });
                    let held = {
                        let mut state = fake.state();
                        state.output = Some(output);
                        state.hold_server.take()
                    };
                    if let Some(held) = held {
                        let _ = held.started.send(());
                        held.release.await;
                    }
                    Ok(Spawned {
                        process: Process(server),
                        output: receiver,
                    })
                }
                "workspace" => {
                    let code = fake.state().workspace_exit;
                    let _ = output.send(ProcessOutput::Stdout("/home/test/.buzz\n".into()));
                    let _ = output.send(ProcessOutput::Exit(Some(code)));
                    Ok(Spawned {
                        process: Process(Arc::new(Short {
                            fake,
                            output,
                            alive: AtomicBool::new(false),
                        })),
                        output: receiver,
                    })
                }
                "read" => {
                    let path = options.args.first().cloned().unwrap_or_default();
                    let held = {
                        let mut state = fake.state();
                        state.reads.push((options.cwd.clone(), path.clone()));
                        state.hold_read.take()
                    };
                    let short = Arc::new(Short {
                        fake,
                        output: output.clone(),
                        alive: AtomicBool::new(true),
                    });
                    match held {
                        Some(held) => {
                            let _ = held.started.send(());
                            held.release.await;
                        }
                        None => {
                            let content: &[u8] = if path == "attachment.jpg" {
                                &[0xff, 0xd8, 0xff]
                            } else {
                                b"FILE CONTENT"
                            };
                            let text = base64::engine::general_purpose::STANDARD.encode(content);
                            let _ = output.send(ProcessOutput::Stdout(text));
                            short.alive.store(false, Ordering::SeqCst);
                            let _ = output.send(ProcessOutput::Exit(Some(0)));
                        }
                    }
                    Ok(Spawned {
                        process: Process(short),
                        output: receiver,
                    })
                }
                other => Err(format!("Process {other} is not declared")),
            }
        })
    }
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

/// Lets spawned tasks run.
pub async fn flush() {
    tokio::time::sleep(Duration::from_millis(30)).await;
}
