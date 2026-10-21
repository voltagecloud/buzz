//! Scripted processes and release listings for setup tests.

use super::Fetch;
use crate::agent_plugins::process::{Process, ProcessControl, Spawn, SpawnOptions, Spawned};
use buzz_host::api::ProcessOutput;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// What a scripted process prints and exits with, or the spawn error.
#[derive(Clone)]
pub enum Script {
    Run {
        stdout: &'static str,
        stderr: &'static str,
        code: i32,
    },
    Fail(&'static str),
    /// Runs until killed.
    Hang,
}

pub fn run(stdout: &'static str, code: i32) -> Script {
    Script::Run {
        stdout,
        stderr: "",
        code,
    }
}

struct Killable(mpsc::UnboundedSender<ProcessOutput>, Arc<AtomicBool>);

impl ProcessControl for Killable {
    fn write(&self, _: String, _: bool) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn kill(&self) -> BoxFuture<'static, Result<(), String>> {
        self.1.store(true, Ordering::SeqCst);
        let _ = self.0.send(ProcessOutput::Exit(None));
        Box::pin(async { Ok(()) })
    }
}

/// Each process started: `id args`, and the release it was pinned to.
pub type Calls = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A spawn whose processes print `scripts["id args"]` and exit with its
/// code; unscripted ones exit 0. A process whose id is gated waits for the
/// gate before it is returned.
#[derive(Clone, Default)]
pub struct Scripted {
    pub scripts: Arc<Mutex<HashMap<String, Script>>>,
    /// Each process started: `id args`, and the release it was pinned to.
    pub calls: Calls,
    pub gates: Arc<Mutex<HashMap<String, oneshot::Receiver<()>>>>,
    pub killed: Arc<AtomicBool>,
}

impl Scripted {
    pub fn new(scripts: &[(&str, Script)]) -> Self {
        let this = Self::default();
        for (line, script) in scripts {
            this.set(line, script.clone());
        }
        this
    }

    pub fn set(&self, line: &str, script: Script) {
        self.scripts
            .lock()
            .unwrap()
            .insert(line.to_string(), script);
    }

    /// Holds processes started as `id` until the returned sender fires.
    pub fn gate(&self, id: &str) -> oneshot::Sender<()> {
        let (open, gate) = oneshot::channel();
        self.gates.lock().unwrap().insert(id.to_string(), gate);
        open
    }

    pub fn lines(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(line, _)| line.clone())
            .collect()
    }
}

impl Spawn for Scripted {
    fn spawn(
        &self,
        id: &str,
        options: SpawnOptions,
    ) -> BoxFuture<'static, Result<Spawned, String>> {
        let line = format!("{id} {}", options.args.join(" "))
            .trim()
            .to_string();
        let pinned = options
            .env
            .iter()
            .find(|(name, _)| name.ends_with("_VERSION") && name.starts_with("BUZZ_"))
            .and_then(|(_, value)| value.clone());
        self.calls.lock().unwrap().push((line.clone(), pinned));
        let script = self.scripts.lock().unwrap().get(&line).cloned();
        let gate = self.gates.lock().unwrap().remove(id);
        let killed = self.killed.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            let (output, receiver) = mpsc::unbounded_channel();
            match script {
                Some(Script::Fail(error)) => return Err(error.into()),
                Some(Script::Run {
                    stdout,
                    stderr,
                    code,
                }) => {
                    let _ = output.send(ProcessOutput::Stdout(stdout.into()));
                    let _ = output.send(ProcessOutput::Stderr(stderr.into()));
                    let _ = output.send(ProcessOutput::Exit(Some(code)));
                }
                Some(Script::Hang) => {}
                None => {
                    let _ = output.send(ProcessOutput::Exit(Some(0)));
                }
            }
            Ok(Spawned {
                process: Process(Arc::new(Killable(output, killed))),
                output: receiver,
            })
        })
    }
}

/// Answers each URL with its status and body; others with 404.
pub fn fetch(pages: &[(&str, u16, &str)]) -> Fetch {
    let pages: HashMap<String, (u16, String)> = pages
        .iter()
        .map(|(url, status, body)| (url.to_string(), (*status, body.to_string())))
        .collect();
    Arc::new(move |url| {
        let page = pages.get(&url).cloned().unwrap_or((404, String::new()));
        Box::pin(async move { Ok(page) })
    })
}
