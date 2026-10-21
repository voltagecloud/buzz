//! Long-lived processes an agent type starts on this host, a port of
//! buzz-app's `src-tauri/src/host_process.rs`.
//!
//! Only the processes a compiled-in agent type declares can run, with their
//! declared arguments first. Stdin stays open for the GUI, and stdout,
//! stderr and the exit code stream back to the connection that started the
//! process, as [`HostAsk::ProcessOutput`], in order. A process belongs to
//! that connection: it is killed when the GUI asks, when the connection
//! closes, or when the host stops. It runs with the host user's own access:
//! there is no sandbox.

use crate::api::{HostAnswer, HostAsk, HostProtocol, ProcessOutput, Request, Response};
use buzz_host_rpc::{Peer, RpcError};
use std::collections::{BTreeMap, HashMap};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

/// Processes alive at once across all connections.
const MAX_PROCESSES: usize = 64;
/// One write to a process's stdin.
const MAX_WRITE_BYTES: usize = 1024 * 1024;
const READ_BYTES: usize = 64 * 1024;
/// Output sent to the GUI in one ask, at most.
const BATCH_BYTES: usize = 1024 * 1024;
/// Reads queued for the GUI before the pipes stop being read, so a process
/// that writes faster than the GUI takes it waits rather than piling up
/// output here.
const QUEUED_READS: usize = 32;
/// How long a process asked to stop has before it is killed outright.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// A process an agent type may start: `(type, id, program, args)`.
type Declared = (
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
);

/// Buzz's own copies of Claude and Codex, apart from any the user
/// installed, kept on the latest release. buzz-app's plugins pass these
/// scripts to a declared `bash -c`; the host pins them instead, so it
/// still runs only what it declares.
const CLAUDE_SH: &str = include_str!("processes/claude.sh");
const CODEX_SH: &str = include_str!("processes/codex.sh");

/// The Claude and Codex types' processes, as their manifests declare
/// them. `claude`, `codex` and `app-server` start the copy in use;
/// `install` and `use` download a release and switch to it.
const DECLARED: &[Declared] = &[
    (
        "claude-code",
        "claude",
        "bash",
        &["-c", CLAUDE_SH, "claude.sh", "run"],
    ),
    (
        "claude-code",
        "install",
        "bash",
        &["-c", CLAUDE_SH, "claude.sh", "install"],
    ),
    (
        "claude-code",
        "use",
        "bash",
        &["-c", CLAUDE_SH, "claude.sh", "use"],
    ),
    ("claude-code", "platform", "uname", &["-sm"]),
    ("claude-code", "read", "base64", &["-i"]),
    // Prints the agent's workspace, which names its Claude project folder.
    ("claude-code", "workspace", "pwd", &[]),
    // Lists the session files that mention a conversation. A fixed search,
    // where `find` would also allow `-exec` or `-delete`.
    (
        "claude-code",
        "find",
        "grep",
        &["-rlF", "--include=*.jsonl", "-e"],
    ),
    // Prints the default workspace once the host has expanded and made it.
    ("codex", "workspace", "pwd", &[]),
    (
        "codex",
        "codex",
        "bash",
        &["-c", CODEX_SH, "codex.sh", "run"],
    ),
    (
        "codex",
        "app-server",
        "bash",
        &["-c", CODEX_SH, "codex.sh", "run", "app-server"],
    ),
    (
        "codex",
        "install",
        "bash",
        &["-c", CODEX_SH, "codex.sh", "install"],
    ),
    ("codex", "use", "bash", &["-c", CODEX_SH, "codex.sh", "use"]),
    ("codex", "platform", "uname", &["-sm"]),
    ("codex", "read", "base64", &["-i"]),
];

pub(crate) fn is_process_request(request: &Request) -> bool {
    matches!(
        request,
        Request::ProcessSpawn { .. } | Request::ProcessWrite { .. } | Request::ProcessKill { .. }
    )
}

struct Entry {
    peer: Peer<HostProtocol>,
    /// The GUI's handle, unique on its connection.
    handle: u64,
    /// Its process group, which is its own pid.
    #[cfg_attr(not(unix), allow(dead_code))]
    group: Option<u32>,
    stdin: Option<Arc<tokio::sync::Mutex<ChildStdin>>>,
    stop: Option<oneshot::Sender<()>>,
}

impl Entry {
    fn stop(&mut self) {
        self.stdin = None;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Every process this host runs for its GUIs. Cheap to clone.
#[derive(Clone, Default)]
pub(crate) struct Processes {
    inner: Arc<Mutex<Registry>>,
}

#[derive(Default)]
struct Registry {
    next: u64,
    entries: HashMap<u64, Entry>,
}

impl Processes {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The entry `peer` started as `handle`.
    fn owned<T>(
        &self,
        peer: &Peer<HostProtocol>,
        handle: u64,
        take: impl FnOnce(&mut Entry) -> T,
    ) -> Result<T, String> {
        let mut registry = self.registry();
        registry
            .entries
            .values_mut()
            .find(|entry| entry.handle == handle && entry.peer.same_connection(peer))
            .map(take)
            .ok_or_else(|| "No such process".into())
    }

    pub(crate) async fn serve(
        &self,
        peer: Peer<HostProtocol>,
        request: Request,
    ) -> Result<Response, RpcError> {
        let result = match request {
            Request::ProcessSpawn {
                handle,
                owner,
                process,
                args,
                cwd,
                env,
            } => self.spawn(peer, handle, &owner, &process, args, cwd, env),
            Request::ProcessWrite {
                handle,
                data,
                close,
            } => self.write(&peer, handle, data, close).await,
            Request::ProcessKill { handle } => self.kill(&peer, handle),
            _ => Err("Not a process request".into()),
        };
        result
            .map(|()| Response::ProcessDone)
            .map_err(RpcError::Failed)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &self,
        peer: Peer<HostProtocol>,
        handle: u64,
        owner: &str,
        process: &str,
        args: Vec<String>,
        cwd: Option<String>,
        env: BTreeMap<String, Option<String>>,
    ) -> Result<(), String> {
        let &(_, _, program, declared) = DECLARED
            .iter()
            .find(|(kind, id, _, _)| *kind == owner && *id == process)
            .ok_or("Process is not declared")?;
        if args.len() > 256 || args.iter().any(|arg| arg.contains('\0')) {
            return Err("Invalid process arguments".into());
        }
        let path = effective_path();
        let mut command = Command::new(buzz_desktop::process::resolve_program(program, &path));
        command
            .args(declared)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Never pass on an identity the host itself was started with.
        for (name, _) in std::env::vars_os() {
            let text = name.to_string_lossy();
            if text.starts_with("BUZZ_") || text.starts_with("NOSTR_") {
                command.env_remove(&name);
            }
        }
        command.env("PATH", &path);
        for (name, value) in env {
            if name.is_empty() || name.contains(['=', '\0']) {
                return Err("Invalid environment variable".into());
            }
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        if let Some(cwd) = cwd {
            let directory = expand_home(&cwd)?;
            std::fs::create_dir_all(&directory)
                .map_err(|_| "Could not create the process directory")?;
            command.current_dir(directory);
        }
        #[cfg(unix)]
        command.as_std_mut().process_group(0);
        self.start(peer, handle, program, command)
    }

    fn start(
        &self,
        peer: Peer<HostProtocol>,
        handle: u64,
        program: &str,
        mut command: Command,
    ) -> Result<(), String> {
        let state = self.clone();
        let mut registry = self.registry();
        if registry.entries.len() >= MAX_PROCESSES {
            return Err("Too many processes are running".into());
        }
        if registry
            .entries
            .values()
            .any(|entry| entry.handle == handle && entry.peer.same_connection(&peer))
        {
            return Err("That process handle is in use".into());
        }
        #[cfg(not(windows))]
        let mut child = command
            .spawn()
            .map_err(|error| format!("Could not start {program}: {error}"))?;
        // Windows has no process groups: a job holds the process and all
        // it starts, and ends them when it closes. No console window opens
        // beside the app.
        #[cfg(windows)]
        let job = buzz_desktop::process::windows_job::WindowsJob::new()
            .ok_or_else(|| format!("Could not start {program}"))?;
        #[cfg(windows)]
        let mut child = job
            .spawn_hidden(&mut command)
            .ok_or_else(|| format!("Could not start {program}"))?;
        let id = registry.next;
        registry.next += 1;
        let (stop, stopped) = oneshot::channel();
        registry.entries.insert(
            id,
            Entry {
                peer: peer.clone(),
                handle,
                group: child.id(),
                stdin: child
                    .stdin
                    .take()
                    .map(|stdin| Arc::new(tokio::sync::Mutex::new(stdin))),
                stop: Some(stop),
            },
        );
        drop(registry);

        // Both pipes feed one queue, so the GUI sees output in the order it
        // was read, then the exit.
        let (output, queue) = mpsc::channel(QUEUED_READS);
        let (gone, listener_gone) = oneshot::channel::<()>();
        tokio::spawn(forward_to_peer(peer.clone(), handle, queue, gone));
        let readers = [
            child
                .stdout
                .take()
                .map(|out| tokio::spawn(read_pipe(out, output.clone(), ProcessOutput::Stdout))),
            child
                .stderr
                .take()
                .map(|err| tokio::spawn(read_pipe(err, output.clone(), ProcessOutput::Stderr))),
        ];
        #[cfg(unix)]
        let group = child.id().map(|pid| pid as i32);
        tokio::spawn(async move {
            let mut stopped = stopped;
            let mut listener_gone = listener_gone;
            let closed = peer.closed();
            tokio::pin!(closed);
            let status = tokio::select! {
                status = child.wait() => status.ok(),
                _ = async {
                    // Only a sent signal counts: a dropped sender is not a
                    // request to stop.
                    tokio::select! {
                        Ok(()) = &mut stopped => {},
                        Ok(()) = &mut listener_gone => {},
                        () = &mut closed => {},
                        else => std::future::pending::<()>().await,
                    }
                } => {
                    #[cfg(unix)]
                    if let Some(group) = group {
                        // SAFETY: signals a process group this module started.
                        unsafe { libc::kill(-group, libc::SIGTERM) };
                    }
                    // No polite stop on Windows: end it and what it started.
                    #[cfg(windows)]
                    job.terminate(STOP_GRACE).await;
                    match tokio::time::timeout(STOP_GRACE, child.wait()).await {
                        Ok(status) => status.ok(),
                        Err(_) => {
                            let _ = child.start_kill();
                            child.wait().await.ok()
                        }
                    }
                }
            };
            // Descendants that outlived it go with it.
            #[cfg(unix)]
            if let Some(group) = group {
                // SAFETY: as above.
                unsafe { libc::kill(-group, libc::SIGKILL) };
            }
            #[cfg(windows)]
            job.terminate(STOP_GRACE).await;
            for reader in readers.into_iter().flatten() {
                let _ = reader.await;
            }
            state.registry().entries.remove(&id);
            let exit = ProcessOutput::Exit(status.and_then(|status| status.code()));
            let _ = output.send(exit).await;
        });
        Ok(())
    }

    async fn write(
        &self,
        peer: &Peer<HostProtocol>,
        handle: u64,
        data: String,
        close: bool,
    ) -> Result<(), String> {
        if data.len() > MAX_WRITE_BYTES {
            return Err("Write is too large".into());
        }
        let stdin = self.owned(peer, handle, |entry| {
            if close {
                entry.stdin.take()
            } else {
                entry.stdin.clone()
            }
        })?;
        let stdin = stdin.ok_or("The process input is closed")?;
        let mut stdin = stdin.lock().await;
        let written = async {
            stdin.write_all(data.as_bytes()).await?;
            stdin.flush().await?;
            if close {
                stdin.shutdown().await?;
            }
            Ok::<_, std::io::Error>(())
        };
        written
            .await
            .map_err(|_| "The process is not reading its input".into())
    }

    fn kill(&self, peer: &Peer<HostProtocol>, handle: u64) -> Result<(), String> {
        self.owned(peer, handle, Entry::stop)
    }

    /// Stops every process: the host is going away.
    pub(crate) fn stop_all(&self) {
        let mut registry = self.registry();
        for entry in registry.entries.values_mut() {
            entry.stop();
        }
        #[cfg(unix)]
        for group in registry.entries.values().filter_map(|entry| entry.group) {
            // SAFETY: as above; the host's tasks may not run again.
            unsafe { libc::kill(-(group as i32), libc::SIGKILL) };
        }
    }
}

/// Sends queued output to the GUI, batching what piled up while the last
/// ask was in flight. A GUI that stops taking it stops the process.
async fn forward_to_peer(
    peer: Peer<HostProtocol>,
    handle: u64,
    mut queue: mpsc::Receiver<ProcessOutput>,
    gone: oneshot::Sender<()>,
) {
    let mut exited = false;
    while !exited {
        let Some(first) = queue.recv().await else {
            return;
        };
        let mut output = vec![first];
        let mut size = 0;
        while size < BATCH_BYTES
            && !matches!(output.last(), Some(ProcessOutput::Exit(_)))
            && let Ok(next) = queue.try_recv()
        {
            if let ProcessOutput::Stdout(text) | ProcessOutput::Stderr(text) = &next {
                size += text.len();
            }
            output.push(next);
        }
        exited = matches!(output.last(), Some(ProcessOutput::Exit(_)));
        match peer
            .request(HostAsk::ProcessOutput { handle, output })
            .await
        {
            Ok(HostAnswer::ProcessReceived) => {}
            _ => {
                let _ = gone.send(());
                return;
            }
        }
    }
}

/// Reads one pipe as text into the queue.
async fn read_pipe(
    mut pipe: impl AsyncRead + Unpin,
    queue: mpsc::Sender<ProcessOutput>,
    wrap: fn(String) -> ProcessOutput,
) {
    let mut buffer = vec![0; READ_BYTES];
    let mut pending = Vec::new();
    loop {
        let read = match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        pending.extend_from_slice(&buffer[..read]);
        let data = take_text(&mut pending);
        if !data.is_empty() && queue.send(wrap(data)).await.is_err() {
            break;
        }
    }
    if !pending.is_empty() {
        let _ = queue
            .send(wrap(String::from_utf8_lossy(&pending).into_owned()))
            .await;
    }
}

/// The complete UTF-8 text in `pending`, leaving a character split across
/// reads for the next one. Invalid bytes become U+FFFD.
fn take_text(pending: &mut Vec<u8>) -> String {
    let complete = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        // An incomplete trailing sequence waits for its remaining bytes.
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => pending.len(),
    };
    let rest = pending.split_off(complete);
    let text = String::from_utf8_lossy(pending).into_owned();
    *pending = rest;
    text
}

/// The `PATH` processes start with. `~/.local/bin`, where per-user tools
/// install, is often missing from a service's `PATH`, so it is added on
/// every platform, not only on macOS as buzz-app does. So is NixOS's
/// system profile, which holds the `curl`, `tar` and `flock` the copies'
/// scripts need when a service's `PATH` has only the bare essentials.
fn effective_path() -> std::ffi::OsString {
    let path = buzz_desktop::process::effective_path();
    let mut directories: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|directory| !directory.as_os_str().is_empty())
        .collect();
    let extra = home()
        .map(|home| home.join(".local/bin"))
        .into_iter()
        .chain(Some(PathBuf::from("/run/current-system/sw/bin")).filter(|path| path.is_dir()));
    for directory in extra {
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    std::env::join_paths(directories).unwrap_or(path)
}

/// An absolute directory, or one under the user's home written `~/…`.
fn expand_home(directory: &str) -> Result<PathBuf, String> {
    let path = if directory == "~" {
        home()?
    } else if let Some(rest) = directory.strip_prefix("~/") {
        home()?.join(rest)
    } else {
        PathBuf::from(directory)
    };
    if !path.is_absolute() {
        return Err("The process directory must be absolute".into());
    }
    Ok(path)
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "No home directory".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_waits_for_split_characters() {
        let mut pending = "é".as_bytes()[..1].to_vec();
        assert_eq!(take_text(&mut pending), "");
        pending.push("é".as_bytes()[1]);
        assert_eq!(take_text(&mut pending), "é");
        assert!(pending.is_empty());
    }

    #[test]
    fn directories_expand_home_and_must_be_absolute() {
        let home = home().unwrap();
        assert_eq!(expand_home("~").unwrap(), home);
        assert_eq!(expand_home("~/.buzz").unwrap(), home.join(".buzz"));
        assert_eq!(expand_home("/tmp").unwrap(), PathBuf::from("/tmp"));
        assert!(expand_home("relative").is_err());
        assert!(expand_home("~other").is_err());
    }

    #[test]
    fn only_declared_processes() {
        assert!(!DECLARED.iter().any(|(_, _, program, _)| *program == "sh"));
        // Every `bash` runs a script the host pins, never one it is sent.
        for (_, _, program, args) in DECLARED {
            if *program == "bash" {
                assert!(args[0] == "-c" && [CLAUDE_SH, CODEX_SH].contains(&args[1]));
            }
        }
    }

    /// The copies' scripts, run as the host runs them.
    #[cfg(unix)]
    mod scripts {
        use super::*;

        /// Runs a copy's script step with its folders under `data`.
        fn step(script: &str, data: &std::path::Path, args: &[&str]) -> std::process::Output {
            std::process::Command::new("bash")
                .args(["-c", script, "script.sh"])
                .args(args)
                .env("XDG_DATA_HOME", data)
                .env("PATH", effective_path())
                .output()
                .unwrap()
        }

        /// A downloaded release of Claude that prints its version.
        fn release(data: &std::path::Path, version: &str) {
            use std::os::unix::fs::PermissionsExt as _;
            let folder = data.join("buzz/claude-code").join(version);
            std::fs::create_dir_all(&folder).unwrap();
            let claude = folder.join("claude");
            std::fs::write(&claude, format!("#!/bin/sh\necho {version} \"$@\"\n")).unwrap();
            std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("buzz-host-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn a_missing_copy_exits_127() {
            let data = scratch("missing");
            let output = step(CLAUDE_SH, &data, &["run", "--version"]);
            assert_eq!(output.status.code(), Some(127));
            assert_eq!(
                String::from_utf8_lossy(&output.stderr),
                "Claude is not installed\n"
            );
            let output = step(CODEX_SH, &data, &["run", "--version"]);
            assert_eq!(output.status.code(), Some(127));
        }

        #[test]
        fn switching_prunes_older_releases_but_keeps_a_newer_one() {
            let data = scratch("prune");
            for version in ["2.1.100", "2.1.121", "2.1.130", "2.1.140"] {
                release(&data, version);
            }
            assert!(step(CLAUDE_SH, &data, &["use", "2.1.121"]).status.success());
            assert!(step(CLAUDE_SH, &data, &["use", "2.1.130"]).status.success());
            let dir = data.join("buzz/claude-code");
            let mut kept: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            kept.sort();
            // The new release, the one before it, and a newer one another build
            // may be testing.
            assert_eq!(kept, ["2.1.121", "2.1.130", "2.1.140"]);
            let output = step(CLAUDE_SH, &data, &["run", "--version"]);
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                "2.1.130 --version\n"
            );
            // A release named to test it runs before it is in use.
            let output = std::process::Command::new("bash")
                .args(["-c", CLAUDE_SH, "claude.sh", "run"])
                .env("XDG_DATA_HOME", &data)
                .env("BUZZ_CLAUDE_VERSION", "2.1.140")
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&output.stdout), "2.1.140\n");
        }

        #[test]
        fn refuses_versions_that_escape_the_folder() {
            let data = scratch("escape");
            let output = step(CLAUDE_SH, &data, &["use", "../x"]);
            assert_eq!(output.status.code(), Some(2));
            let output = step(CODEX_SH, &data, &["install", "1.0.0", "evil", "00"]);
            assert_eq!(
                String::from_utf8_lossy(&output.stderr),
                "Unsupported platform\n"
            );
        }
    }
}
