//! A running plugin program: JSON-RPC lines over its stdin and stdout.
//!
//! The program runs with the user's full access, as buzz-app's
//! `host.processes` do; nothing here limits what it can reach. Stopping it
//! closes stdin, waits a moment for it to exit, then ends its process
//! group (a job on Windows), so nothing it started outlives it.

use super::protocol::{self, Incoming, MAX_LINE, Manifest};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

/// How long a request to the plugin may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a stopped plugin has to exit after its stdin closes.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// Stderr lines kept to explain an exit.
const STDERR_TAIL: usize = 20;

/// A request's error when the plugin ended first; [`Event::Exited`]
/// follows with the reason.
pub const STOPPED: &str = "The plugin stopped.";

/// What the plugin did, for the UI thread.
#[derive(Debug)]
pub enum Event {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// It exited without being asked to.
    Exited(String),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct Connection {
    lines: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: Arc<AtomicU64>,
    stop: Option<oneshot::Sender<()>>,
    runtime: tokio::runtime::Handle,
}

/// The Node that runs `node` commands: `BUZZ_NODE`, else the one
/// installed beside the app (`../lib/buzz-gpui/node/bin/node` on Linux
/// and Windows, `../Resources/node/bin/node` in a macOS app). `None`
/// finds it on `PATH`.
fn bundled_node() -> Option<std::path::PathBuf> {
    if let Some(node) = std::env::var_os("BUZZ_NODE") {
        return Some(node.into());
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let root = exe.parent()?.parent()?;
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    [
        root.join("lib/buzz-gpui/node/bin").join(name),
        root.join("Resources/node/bin").join(name),
    ]
    .into_iter()
    .find(|node| node.is_file())
}

impl Connection {
    /// Starts the plugin in `folder`.
    pub fn start(
        manifest: &Manifest,
        folder: &Path,
        runtime: &tokio::runtime::Handle,
    ) -> Result<(Self, futures::channel::mpsc::UnboundedReceiver<Event>), String> {
        let _guard = runtime.enter();
        let path = buzz_desktop::process::effective_path();
        let program = &manifest.command[0];
        let executable = if program.contains('/') || program.contains('\\') {
            folder.join(program)
        } else if program == "node"
            && let Some(node) = bundled_node()
        {
            node
        } else {
            buzz_desktop::process::resolve_program(program, &path)
        };
        // Somewhere to keep state that is not in the folder, which may be
        // read-only once installed.
        let data = crate::plugins::home().join("data").join(&manifest.id);
        let _ = std::fs::create_dir_all(&data);
        let mut command = Command::new(&executable);
        command
            .args(&manifest.command[1..])
            .current_dir(folder)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Never pass on the app's own identity.
        for (name, _) in std::env::vars_os() {
            let text = name.to_string_lossy();
            if text.starts_with("BUZZ_") || text.starts_with("NOSTR_") {
                command.env_remove(&name);
            }
        }
        command
            .env("PATH", &path)
            .env("BUZZ_PLUGIN_ID", &manifest.id)
            .env("BUZZ_PLUGIN_API_VERSION", protocol::API_VERSION.to_string())
            .env("BUZZ_PLUGIN_DATA_DIR", &data)
            // Python buffers a piped stdout; a plugin that forgets to
            // flush would look hung.
            .env("PYTHONUNBUFFERED", "1");
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(command.as_std_mut(), 0);
        let fail = |error: std::io::Error| format!("Could not start {program}: {error}");
        #[cfg(not(windows))]
        let mut child = command.spawn().map_err(fail)?;
        #[cfg(windows)]
        let job = buzz_desktop::process::windows_job::WindowsJob::new()
            .ok_or_else(|| format!("Could not start {program}"))?;
        #[cfg(windows)]
        let mut child = job
            .spawn_hidden(&mut command)
            .ok_or_else(|| format!("Could not start {program}"))?;
        #[cfg(windows)]
        let _ = fail;

        let (events, received) = futures::channel::mpsc::unbounded();
        let pending: Pending = Arc::default();
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let id = manifest.id.clone();

        let stdout = child.stdout.take().expect("piped stdout");
        let reader = runtime.spawn(read_stdout(
            id.clone(),
            BufReader::new(stdout),
            pending.clone(),
            events.clone(),
        ));
        let stderr = child.stderr.take().expect("piped stderr");
        let errors = runtime.spawn(read_stderr(
            id.clone(),
            BufReader::new(stderr),
            tail.clone(),
        ));

        let (lines, mut outgoing) = mpsc::unbounded_channel::<String>();
        let mut stdin = child.stdin.take().expect("piped stdin");
        runtime.spawn(async move {
            while let Some(line) = outgoing.recv().await {
                let written = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                };
                if written.await.is_err() {
                    break;
                }
            }
            // Dropping stdin tells the plugin to exit.
        });

        let (stop, stopped) = oneshot::channel();
        let group = child.id();
        runtime.spawn(async move {
            #[cfg(windows)]
            let _job = job;
            let status = tokio::select! {
                status = child.wait() => Some(status),
                _ = stopped => None,
            };
            let status = match status {
                Some(status) => status,
                None => {
                    // Asked to stop: stdin is closing; give it a moment.
                    let status = tokio::time::timeout(EXIT_GRACE, child.wait()).await;
                    end_group(group);
                    if status.is_err() {
                        let _ = child.kill().await;
                    }
                    reader.abort();
                    return;
                }
            };
            end_group(group);
            // Let the readers take in what the plugin wrote last.
            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                let _ = reader.await;
                let _ = errors.await;
            })
            .await;
            let mut reason = match status {
                Ok(status) => match status.code() {
                    Some(code) => format!("The plugin exited with code {code}."),
                    None => "The plugin was stopped by a signal.".to_owned(),
                },
                Err(error) => format!("The plugin could not be watched: {error}"),
            };
            let tail = tail.lock().unwrap();
            if !tail.is_empty() {
                reason.push('\n');
                reason.push_str(&tail.iter().cloned().collect::<Vec<_>>().join("\n"));
            }
            let _ = events.unbounded_send(Event::Exited(reason));
        });

        Ok((
            Self {
                lines,
                pending,
                next: Arc::new(AtomicU64::new(1)),
                stop: Some(stop),
                runtime: runtime.clone(),
            },
            received,
        ))
    }

    /// Sends a request; the answer comes within [`REQUEST_TIMEOUT`].
    pub fn request(
        &self,
        method: &str,
        params: Value,
    ) -> tokio::task::JoinHandle<Result<Value, String>> {
        self.requester().request(method, params, REQUEST_TIMEOUT)
    }

    /// A handle that sends requests from any thread while this run lasts.
    pub fn requester(&self) -> Requester {
        Requester {
            lines: self.lines.clone(),
            pending: self.pending.clone(),
            next: self.next.clone(),
            runtime: self.runtime.clone(),
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.lines.send(protocol::notification(method, params));
    }

    pub fn respond(&self, id: &Value, result: Result<Value, (i64, String)>) {
        let _ = self.lines.send(protocol::response(id, result));
    }
}

/// Sends requests to one run of a plugin; they fail once it stops.
#[derive(Clone)]
pub struct Requester {
    lines: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: Arc<AtomicU64>,
    runtime: tokio::runtime::Handle,
}

impl Requester {
    /// The runtime its requests run on.
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.lines.send(protocol::notification(method, params));
    }

    /// Sends a request; the answer comes within `timeout`.
    pub fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> tokio::task::JoinHandle<Result<Value, String>> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, answer);
        let _ = self.lines.send(protocol::request(id, method, params));
        let pending = self.pending.clone();
        let method = method.to_owned();
        self.runtime.spawn(async move {
            match tokio::time::timeout(timeout, answered).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(STOPPED.into()),
                Err(_) => {
                    pending.lock().unwrap().remove(&id);
                    Err(format!("The plugin did not answer {method} in time."))
                }
            }
        })
    }
}

impl Drop for Connection {
    /// Closes stdin and ends the program.
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Ends what the plugin started, now that it is done.
fn end_group(group: Option<u32>) {
    #[cfg(unix)]
    if let Some(group) = group.and_then(|id| i32::try_from(id).ok()) {
        // SAFETY: signals a process group this app created.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = group;
}

/// Reads one line of at most [`MAX_LINE`] bytes, without the newline.
/// `Ok(None)` at the end of the stream.
async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut Vec<u8>,
) -> std::io::Result<Option<()>> {
    line.clear();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok((!line.is_empty()).then_some(()));
        }
        let (taken, done) = match buffer.iter().position(|byte| *byte == b'\n') {
            Some(end) => (end + 1, true),
            None => (buffer.len(), false),
        };
        if line.len() + taken > MAX_LINE + 1 {
            return Err(std::io::Error::other("a line over 4 MiB"));
        }
        line.extend_from_slice(&buffer[..taken]);
        reader.consume(taken);
        if done {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(()));
        }
    }
}

async fn read_stdout(
    id: String,
    mut reader: impl AsyncBufRead + Unpin,
    pending: Pending,
    events: futures::channel::mpsc::UnboundedSender<Event>,
) {
    let mut line = Vec::new();
    loop {
        match read_line(&mut reader, &mut line).await {
            Ok(Some(())) => {}
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(plugin = %id, %error, "plugin output unreadable");
                break;
            }
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            tracing::warn!(plugin = %id, "plugin wrote a line that is not UTF-8");
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let event = match Incoming::parse(text) {
            Ok(Incoming::Response { id, result }) => {
                if let Some(answer) = pending.lock().unwrap().remove(&id) {
                    let _ = answer.send(result);
                }
                continue;
            }
            Ok(Incoming::Request { id, method, params }) => Event::Request { id, method, params },
            Ok(Incoming::Notification { method, params }) => Event::Notification { method, params },
            Err(error) => {
                // Stray prints go to stdout by mistake; say so and go on.
                tracing::warn!(plugin = %id, %error, line = %truncate(text), "ignored plugin output");
                continue;
            }
        };
        if events.unbounded_send(event).is_err() {
            break;
        }
    }
    // Requests still waiting will not be answered.
    pending.lock().unwrap().clear();
}

async fn read_stderr(
    id: String,
    mut reader: impl AsyncBufRead + Unpin,
    tail: Arc<Mutex<VecDeque<String>>>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = truncate(line.trim_end());
        tracing::info!(plugin = %id, "{text}");
        let mut tail = tail.lock().unwrap();
        if tail.len() == STDERR_TAIL {
            tail.pop_front();
        }
        tail.push_back(text);
    }
}

fn truncate(text: &str) -> String {
    const MAX: usize = 500;
    match text.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn lines_are_split_and_bounded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut reader = BufReader::new(&b"one\r\ntwo\nthree"[..]);
            let mut line = Vec::new();
            let mut lines = Vec::new();
            while read_line(&mut reader, &mut line).await.unwrap().is_some() {
                lines.push(String::from_utf8(line.clone()).unwrap());
            }
            assert_eq!(lines, ["one", "two", "three"]);

            let long = vec![b'x'; MAX_LINE + 10];
            let mut reader = BufReader::new(&long[..]);
            assert!(read_line(&mut reader, &mut line).await.is_err());
        });
    }

    /// Runs `examples/plugins/hello` over real pipes: initialize, open a
    /// page, click, then stop.
    #[test]
    fn the_example_plugin_answers_over_stdio() {
        use futures::StreamExt;
        use serde_json::json;

        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipped: no python3");
            return;
        }
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/hello");
        let manifest =
            Manifest::parse(&std::fs::read_to_string(folder.join(protocol::MANIFEST)).unwrap())
                .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (connection, mut events) =
            Connection::start(&manifest, &folder, runtime.handle()).unwrap();
        runtime.block_on(async {
            let initialized = connection
                .request("initialize", json!({ "apiVersion": 1 }))
                .await
                .unwrap()
                .unwrap();
            let contributions = protocol::Contributions::parse(initialized).unwrap();
            assert_eq!(contributions.pages[0].id, "hello");
            assert_eq!(contributions.message_actions.len(), 2);
            assert_eq!(contributions.shortcuts[0].keys, ["mod+shift+h"]);
            assert_eq!(contributions.panels[0].matches, ["https://example.com/"]);

            connection.notify(
                "view/open",
                json!({ "view": "v1", "contribution": "hello", "kind": "page" }),
            );
            let mut next = async || loop {
                let event = tokio::time::timeout(Duration::from_secs(10), events.next())
                    .await
                    .expect("the plugin answered")
                    .expect("the plugin is running");
                // The Greeter's readiness is not what these steps await.
                match &event {
                    Event::Notification { method, .. } if method == "agentType/readiness" => {}
                    _ => return event,
                }
            };
            let tree = |event: Event| match event {
                Event::Notification { method, params } if method == "view/render" => {
                    protocol::Node::parse(params["tree"].clone()).unwrap();
                    params["tree"].to_string()
                }
                other => panic!("unexpected {other:?}"),
            };
            assert!(tree(next().await).contains("Clicked 0 times"));
            // The page asks for the channels; answer it.
            let Event::Request { id, method, .. } = next().await else {
                panic!("no channel request");
            };
            assert_eq!(method, "community/channels");
            // And who the user is, for its avatar.
            let Event::Request {
                id: current,
                method,
                ..
            } = next().await
            else {
                panic!("no community request");
            };
            assert_eq!(method, "community/current");
            connection.respond(&current, Ok(serde_json::Value::Null));
            connection.respond(
                &id,
                Ok(json!([{ "id": "c1", "name": "general", "kind": "stream", "private": false }])),
            );
            assert!(tree(next().await).contains("general"));
            connection.notify(
                "view/event",
                json!({ "view": "v1", "id": "add", "event": "click" }),
            );
            // The page's badge counts the clicks too.
            let Event::Notification { method, params } = next().await else {
                panic!("no badge");
            };
            assert_eq!(
                (method.as_str(), &params["text"]),
                ("page/badge", &json!("1"))
            );
            assert!(tree(next().await).contains("Clicked 1 times"));

            // Renderers answer requests with trees.
            let chip = connection
                .request(
                    "link/render",
                    json!({ "renderer": "example-link", "url": "https://example.com/docs" }),
                )
                .await
                .unwrap()
                .unwrap();
            protocol::Node::parse(chip["tree"].clone()).unwrap();
            assert!(chip.to_string().contains("Example · /docs"));
            let ticket = connection
                .request(
                    "inline/render",
                    json!({ "renderer": "tickets", "text": "HELLO-42", "message": {} }),
                )
                .await
                .unwrap()
                .unwrap();
            protocol::Node::parse(ticket["tree"].clone()).unwrap();
            let plain = connection
                .request(
                    "inline/render",
                    json!({ "renderer": "tickets", "text": "HELLO-0", "message": {} }),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(plain, serde_json::Value::Null);
            let agent = json!({ "pubkey": "ab", "name": "Gus", "owner": "cd",
                "config": { "greeting": "Howdy" } });
            let summary = connection
                .request(
                    "agent/summary",
                    json!({ "type": "greeter", "agent": agent }),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(summary["text"], "Says “Howdy”");
            let peek = connection
                .request("agent/peek", json!({ "type": "greeter", "agent": agent }))
                .await
                .unwrap()
                .unwrap();
            assert!(protocol::Node::parse(peek["tree"].clone()).is_ok());
            let card = connection
                .request(
                    "message/render",
                    json!({ "renderer": "hello-card", "message": {
                        "content": "!hello *there*", "authorName": "Ada",
                    } }),
                )
                .await
                .unwrap()
                .unwrap();
            assert!(card.to_string().contains("Ada says hello"), "{card}");
        });
        // Dropping the connection closes stdin; the plugin exits without
        // reporting a crash.
        drop(connection);
        runtime.block_on(async {
            let event = tokio::time::timeout(Duration::from_secs(5), events.next()).await;
            assert!(!matches!(event, Ok(Some(Event::Exited(_)))), "{event:?}");
        });
    }

    #[test]
    fn a_crash_is_reported_with_its_stderr() {
        use futures::StreamExt;

        let folder = std::env::temp_dir().join(format!("buzz-plugin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let manifest = Manifest::parse(
            r#"{ "id": "com.example.crash", "name": "Crash",
                 "command": ["sh", "-c", "echo not json; echo broken >&2; exit 3"] }"#,
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (connection, mut events) =
            Connection::start(&manifest, &folder, runtime.handle()).unwrap();
        runtime.block_on(async {
            let event = tokio::time::timeout(Duration::from_secs(10), events.next())
                .await
                .unwrap();
            let Some(Event::Exited(reason)) = event else {
                panic!("{event:?}");
            };
            assert!(
                reason.contains("code 3") && reason.contains("broken"),
                "{reason}"
            );
        });
        drop(connection);
        let _ = std::fs::remove_dir_all(folder);
    }
}
