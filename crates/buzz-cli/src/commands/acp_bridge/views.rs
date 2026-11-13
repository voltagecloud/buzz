//! Read-only branch views on the Paseo daemon.
//!
//! Replay points agent file paths at `$HOME/code/<owner>/<repo>@<branch>`
//! (see [`super::workspace_paths`]). The daemon's `paseo-code view <owner>/<repo>
//! <branch>` creates that detached checkout, or moves it to the branch's
//! fetched head. The bridge runs it in the background, up to three times per (repo,
//! branch, head) per process, only when the view is missing or at another
//! commit. Replay never waits for it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;

use super::workspace_paths::ViewKey;

/// Views opened at the same time. Each one fetches from GitHub.
const VIEW_CONCURRENCY: usize = 2;
/// Most distinct views one bridge process requests.
const MAX_VIEW_REQUESTS: usize = 64;
/// Longest a `paseo-code view` may run; it stops its own Git commands after
/// 180 s.
const VIEW_TIMEOUT: Duration = Duration::from_secs(200);
/// Bytes of `paseo-code` error output kept for the log.
const STDERR_TAIL: usize = 2048;

/// Background `paseo-code view` runs for one bridge process.
pub(crate) struct BranchViews {
    program: PathBuf,
    home: PathBuf,
    requested: Mutex<HashSet<ViewKey>>,
    slots: Semaphore,
    /// Set once the program is found missing, so it is reported once.
    unavailable: AtomicBool,
}

impl BranchViews {
    pub(crate) fn new(program: PathBuf, home: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            program,
            home,
            requested: Mutex::new(HashSet::new()),
            slots: Semaphore::new(VIEW_CONCURRENCY),
            unavailable: AtomicBool::new(false),
        })
    }

    /// Open the view of `key` in the background, unless this process already
    /// requested it. Returns at once.
    pub(crate) fn request(self: &Arc<Self>, key: ViewKey) {
        if !cfg!(unix) {
            if !self.unavailable.swap(true, Ordering::Relaxed) {
                eprintln!("acp-bridge: branch views require a Unix Paseo daemon");
            }
            return;
        }
        if self.unavailable.load(Ordering::Relaxed) {
            return;
        }
        {
            let Ok(mut requested) = self.requested.lock() else {
                return;
            };
            if requested.len() >= MAX_VIEW_REQUESTS || !requested.insert(key.clone()) {
                return;
            }
        }
        let views = Arc::clone(self);
        tokio::spawn(async move {
            for attempt in 0..3 {
                if views.open(&key).await || views.unavailable.load(Ordering::Relaxed) {
                    return;
                }
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
            }
            eprintln!("acp-bridge: branch view retries exhausted; reopen the conversation to retry in a new bridge");
        });
    }

    async fn open(&self, key: &ViewKey) -> bool {
        let Some(dir) = key.dir(&self.home) else {
            return false;
        };
        if view_head(&dir).await.as_deref() == Some(key.head.as_str()) {
            return true;
        }
        let Ok(_slot) = self.slots.acquire().await else {
            return false;
        };
        if self.unavailable.load(Ordering::Relaxed) {
            return false;
        }
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(["view", &key.repo, &key.branch, "--inherit-process-group"])
            .stdin(Stdio::null())
            // stdout carries the ACP protocol; never let the child write to it.
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !self.unavailable.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "acp-bridge: {} is not installed; agent files open only where their sandbox path exists",
                        self.program.display()
                    );
                }
                return false;
            }
            Err(error) => {
                eprintln!(
                    "acp-bridge: could not start {}: {error}",
                    self.program.display()
                );
                return false;
            }
        };
        // A timed-out view's Git processes are killed with it.
        #[cfg(unix)]
        let _group = KillGroupOnDrop(child.id().and_then(|id| i32::try_from(id).ok()));
        let stderr = child.stderr.take();
        let run = async {
            let tail = stderr_tail(stderr).await;
            (child.wait().await, tail)
        };
        let label = format!("paseo-code view {} {}", key.repo, key.branch);
        match tokio::time::timeout(VIEW_TIMEOUT, run).await {
            Ok((Ok(status), _)) if status.success() => true,
            Ok((Ok(status), tail)) => {
                let reason = String::from_utf8_lossy(&tail);
                let reason = reason.trim().lines().last().unwrap_or_default();
                eprintln!(
                    "acp-bridge: {label} failed ({status}): {reason}; its files stay unavailable"
                );
                false
            }
            Ok((Err(error), _)) => {
                eprintln!("acp-bridge: {label} failed: {error}");
                false
            }
            Err(_) => {
                eprintln!(
                    "acp-bridge: {label} did not finish within {}s; stopped it",
                    VIEW_TIMEOUT.as_secs()
                );
                false
            }
        }
    }
}

/// The last [`STDERR_TAIL`] bytes of the child's error output, read to its
/// end so the child never blocks on a full pipe.
async fn stderr_tail(stderr: Option<tokio::process::ChildStderr>) -> Vec<u8> {
    let mut tail = Vec::new();
    let Some(mut stderr) = stderr else {
        return tail;
    };
    let mut buf = [0u8; 4096];
    while let Ok(read) = stderr.read(&mut buf).await {
        if read == 0 {
            break;
        }
        tail.extend_from_slice(&buf[..read]);
        if tail.len() > STDERR_TAIL {
            tail.drain(..tail.len() - STDERR_TAIL);
        }
    }
    tail
}

/// The commit a branch view is at: its `.git` file names the worktree's Git
/// directory, whose `HEAD` holds the detached commit. `None` when the view is
/// missing or unreadable.
async fn view_head(dir: &Path) -> Option<String> {
    let link = read_small(&dir.join(".git")).await?;
    let gitdir = link.trim().strip_prefix("gitdir:")?.trim();
    let head = read_small(&Path::new(gitdir).join("HEAD")).await?;
    Some(head.trim().to_owned())
}

async fn read_small(path: &Path) -> Option<String> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut text = String::new();
    file.take(4096).read_to_string(&mut text).await.ok()?;
    Some(text)
}

#[cfg(unix)]
struct KillGroupOnDrop(Option<i32>);

#[cfg(unix)]
impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}
