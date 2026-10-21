//! Processes an agent type starts on the host that holds its agent, the
//! port of buzz-app's `host.spawn` (`src/features/host/service.ts`).
//!
//! The host runs only processes the type declares, and streams their output
//! back to this connection as [`HostAsk::ProcessOutput`]. The GUI picks each
//! handle and registers where its output goes before asking, so nothing
//! that arrives early is lost.

use buzz_host::api::{HostAnswer, HostProtocol, ProcessOutput, Request};
use buzz_host_rpc::{ClientEvent, HostClient, RpcError};
use futures::future::BoxFuture;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::mpsc;

/// How to start a declared process.
#[derive(Clone, Debug, Default)]
pub struct SpawnOptions {
    /// After the arguments the type declares.
    pub args: Vec<String>,
    /// An absolute directory or one under home written `~/…`; created if
    /// missing.
    pub cwd: Option<String>,
    /// Added to the host's environment; `None` removes a variable.
    pub env: BTreeMap<String, Option<String>>,
}

/// A running process: its controls, and its output ending in an exit.
pub struct Spawned {
    pub process: Process,
    pub output: mpsc::UnboundedReceiver<ProcessOutput>,
}

pub trait ProcessControl: Send + Sync {
    /// Writes `data` to stdin; `close` then ends it.
    fn write(&self, data: String, close: bool) -> BoxFuture<'static, Result<(), String>>;
    /// Asks it to stop, then kills it and its descendants.
    fn kill(&self) -> BoxFuture<'static, Result<(), String>>;
}

/// A process's controls. Cheap to clone.
#[derive(Clone)]
pub struct Process(pub Arc<dyn ProcessControl>);

impl Process {
    pub async fn write(&self, data: String) -> Result<(), String> {
        self.0.write(data, false).await
    }
    pub async fn kill(&self) -> Result<(), String> {
        self.0.kill().await
    }
    /// Closes stdin, which asks a server reading it to finish.
    pub async fn end(&self) -> Result<(), String> {
        self.0.write(String::new(), true).await
    }
}

/// Starts an agent type's declared processes.
pub trait Spawn: Send + Sync {
    fn spawn(&self, id: &str, options: SpawnOptions)
    -> BoxFuture<'static, Result<Spawned, String>>;

    /// Which host its processes run on: equal for spawners that start them
    /// in the same place. Spawners that do not say share one.
    fn host(&self) -> usize {
        0
    }
}

pub type Spawner = Arc<dyn Spawn>;

/// Where each process's output goes, by handle, for one host connection.
/// Cheap to clone.
#[derive(Clone, Default)]
pub struct ProcessSinks {
    inner: Arc<Sinks>,
}

#[derive(Default)]
struct Sinks {
    next: AtomicU64,
    sinks: Mutex<HashMap<u64, mpsc::UnboundedSender<ProcessOutput>>>,
}

impl ProcessSinks {
    fn sinks(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<u64, mpsc::UnboundedSender<ProcessOutput>>> {
        self.inner
            .sinks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn open(&self) -> (u64, mpsc::UnboundedReceiver<ProcessOutput>) {
        let handle = self.inner.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = mpsc::unbounded_channel();
        self.sinks().insert(handle, sender);
        (handle, receiver)
    }

    /// Hands output the host sent to its process's reader. Runs as the ask
    /// is read, so output keeps its order. An unknown handle, or a reader
    /// that is gone, tells the host to stop the process.
    pub fn deliver(&self, handle: u64, output: Vec<ProcessOutput>) -> Result<HostAnswer, RpcError> {
        let mut sinks = self.sinks();
        let Some(sink) = sinks.get(&handle) else {
            return Err(RpcError::Failed("No such process".into()));
        };
        let mut exited = false;
        for item in output {
            exited |= matches!(item, ProcessOutput::Exit(_));
            if sink.send(item).is_err() {
                sinks.remove(&handle);
                return Err(RpcError::Failed("Nobody is reading that process".into()));
            }
        }
        if exited {
            sinks.remove(&handle);
        }
        Ok(HostAnswer::ProcessReceived)
    }

    /// The connection dropped, and the host killed this connection's
    /// processes with it: each ends, its exit unknown.
    fn disconnected(&self) {
        for (_, sink) in self.sinks().drain() {
            let _ = sink.send(ProcessOutput::Exit(None));
        }
    }

    /// Ends this connection's processes whenever the connection drops.
    pub fn follow(&self, client: &HostClient<HostProtocol>, runtime: &tokio::runtime::Handle) {
        let sinks = self.clone();
        let mut events = client.events();
        runtime.spawn(async move {
            loop {
                match events.recv().await {
                    Ok(ClientEvent::Disconnected) => sinks.disconnected(),
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        });
    }
}

/// Spawns `owner`'s processes on one host.
pub struct HostSpawner {
    pub client: Arc<HostClient<HostProtocol>>,
    pub sinks: ProcessSinks,
    /// The agent type whose declared processes these are.
    pub owner: &'static str,
}

/// The agent types whose processes hosts run, by owner, as people know
/// them.
const OWNERS: &[(&str, &str)] = &[("claude-code", "Claude"), ("codex", "Codex")];

/// What to tell someone whose host is too old for `owner`'s processes:
/// older than the process bridge, or than the type's declarations.
pub fn update_host(owner: &str) -> String {
    let title = OWNERS
        .iter()
        .find(|(id, _)| *id == owner)
        .map_or(owner, |(_, title)| title);
    format!("Update this host to run {title} agents")
}

fn failure(owner: &str, error: RpcError) -> String {
    match error {
        RpcError::Failed(message)
            if message.contains("does not support this request")
                || message == "Process is not declared" =>
        {
            update_host(owner)
        }
        other => other.to_string(),
    }
}

impl Spawn for HostSpawner {
    fn spawn(
        &self,
        id: &str,
        options: SpawnOptions,
    ) -> BoxFuture<'static, Result<Spawned, String>> {
        let client = self.client.clone();
        let sinks = self.sinks.clone();
        let request = |handle| Request::ProcessSpawn {
            handle,
            owner: self.owner.into(),
            process: id.into(),
            args: options.args,
            cwd: options.cwd,
            env: options.env,
        };
        let (handle, output) = sinks.open();
        let request = request(handle);
        let owner = self.owner;
        Box::pin(async move {
            if let Err(error) = client.request(request).await {
                sinks.sinks().remove(&handle);
                return Err(failure(owner, error));
            }
            Ok(Spawned {
                process: Process(Arc::new(HostProcess {
                    client,
                    handle,
                    owner,
                    writing: Arc::default(),
                })),
                output,
            })
        })
    }

    /// Its connection: every spawner made for one host shares it.
    fn host(&self) -> usize {
        Arc::as_ptr(&self.client) as *const () as usize
    }
}

struct HostProcess {
    client: Arc<HostClient<HostProtocol>>,
    handle: u64,
    owner: &'static str,
    /// Held across the pieces of one write, so no other lands between them.
    writing: Arc<tokio::sync::Mutex<()>>,
}

/// Bytes per process write: hosts take at most 1 MiB at a time.
const WRITE_PIECE: usize = 1024 * 1024;

/// `data` in pieces of at most [`WRITE_PIECE`] bytes, never splitting a
/// character; empty data is one empty piece.
fn pieces(data: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = data;
    while rest.len() > WRITE_PIECE {
        let mut end = WRITE_PIECE;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (piece, tail) = rest.split_at(end);
        pieces.push(piece);
        rest = tail;
    }
    pieces.push(rest);
    pieces
}

impl ProcessControl for HostProcess {
    /// A write longer than the host takes at once goes in pieces, in
    /// order, closing stdin only after the last.
    fn write(&self, data: String, close: bool) -> BoxFuture<'static, Result<(), String>> {
        let (client, writing) = (self.client.clone(), self.writing.clone());
        let (handle, owner) = (self.handle, self.owner);
        Box::pin(async move {
            let _turn = writing.lock().await;
            let pieces = pieces(&data);
            let last = pieces.len() - 1;
            for (index, piece) in pieces.into_iter().enumerate() {
                client
                    .request(Request::ProcessWrite {
                        handle,
                        data: piece.to_owned(),
                        close: close && index == last,
                    })
                    .await
                    .map_err(|error| failure(owner, error))?;
            }
            Ok(())
        })
    }

    fn kill(&self) -> BoxFuture<'static, Result<(), String>> {
        let client = self.client.clone();
        let (handle, owner) = (self.handle, self.owner);
        Box::pin(async move {
            client
                .request(Request::ProcessKill { handle })
                .await
                .map(drop)
                .map_err(|error| failure(owner, error))
        })
    }
}

/// Runs everything a process printed to completion: its exit code (`None`
/// for a signal or a lost host) and its stdout and stderr.
pub async fn collect(
    output: &mut mpsc::UnboundedReceiver<ProcessOutput>,
) -> (Option<i32>, String, String) {
    let (mut stdout, mut stderr) = (String::new(), String::new());
    while let Some(item) = output.recv().await {
        match item {
            ProcessOutput::Stdout(text) => stdout += &text,
            ProcessOutput::Stderr(text) => stderr += &text,
            ProcessOutput::Exit(code) => return (code, stdout, stderr),
        }
    }
    (None, stdout, stderr)
}

/// A process's output split into lines (NDJSON), holding at most `limit`
/// bytes of a line not yet ended.
#[derive(Default)]
pub struct Lines(String);

impl Lines {
    /// The lines `data` completes, each with its line break, and whether
    /// a line ran past `limit`; that line is dropped.
    pub fn push(&mut self, data: &str, limit: usize) -> (Vec<String>, bool) {
        self.0.push_str(data);
        let mut lines = Vec::new();
        while let Some(newline) = self.0.find('\n') {
            if newline > limit {
                break;
            }
            lines.push(self.0.drain(..=newline).collect());
        }
        let oversized = self.0.find('\n').unwrap_or(self.0.len()) > limit;
        if oversized {
            self.0.clear();
        }
        (lines, oversized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_writes_go_in_pieces_the_host_takes_without_splitting_characters() {
        assert_eq!(pieces(""), [""]);
        assert_eq!(pieces("short"), ["short"]);
        let long = format!("{}é{}", "a".repeat(WRITE_PIECE - 1), "b".repeat(10));
        let parts = pieces(&long);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), WRITE_PIECE - 1);
        assert!(parts.iter().all(|part| part.len() <= WRITE_PIECE));
        assert_eq!(parts.concat(), long);
    }

    #[test]
    fn old_hosts_are_asked_to_update_for_the_type() {
        let old = || RpcError::Failed("The other side does not support this request".into());
        assert_eq!(
            failure("claude-code", old()),
            "Update this host to run Claude agents"
        );
        // A host from before Codex knows the bridge but not its processes.
        assert_eq!(
            failure("codex", RpcError::Failed("Process is not declared".into())),
            "Update this host to run Codex agents"
        );
        assert_eq!(
            failure("codex", RpcError::Failed("Could not start codex".into())),
            "Could not start codex"
        );
    }

    #[test]
    fn splits_lines_and_drops_one_past_the_limit() {
        let mut lines = Lines::default();
        assert_eq!(
            lines.push("{\"a\":1}\n{\"b\"", 16),
            (vec!["{\"a\":1}\n".into()], false)
        );
        assert_eq!(lines.push(":2}\n", 16), (vec!["{\"b\":2}\n".into()], false));
        assert_eq!(lines.push(&"x".repeat(17), 16), (vec![], true));
        assert_eq!(lines.push("{}\n", 16), (vec!["{}\n".into()], false));
    }
}
