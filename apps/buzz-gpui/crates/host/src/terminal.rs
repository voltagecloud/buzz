//! Terminals on the host (buzz-app's `terminal_*` commands): the host
//! user's login shell in a PTY, one per channel a GUI opens a terminal in.
//!
//! The PTYs are buzz-app's own (`buzz_desktop::terminal`); this module
//! puts them behind the host protocol. Output is pulled with a long poll:
//! a read returns as soon as the shell has written something, or after
//! the wait the GUI asked for. Pulled output stays in the PTY until read,
//! so nothing is lost the way a lagging event broadcast would lose it,
//! and a slow GUI slows the shell rather than piling output up here.
//!
//! An owner made over a connection is closed when that connection closes,
//! so a GUI that goes away never leaves shells running.

use crate::Asker;
use crate::api::{Request, Response};
use buzz_desktop::terminal as pty;
use buzz_host_rpc::RpcError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The longest a read waits for output.
pub const MAX_WAIT: Duration = Duration::from_secs(5);
/// Polling the non-blocking PTY backs off to this while the shell is quiet.
const QUIET_PAUSE: Duration = Duration::from_millis(16);
/// And restarts at this after output or input, when more output is likely.
const BUSY_PAUSE: Duration = Duration::from_millis(1);

/// What the shell is told about where it was opened (buzz-app's
/// `ShellContext`). Public presentation context only, never credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalContext {
    /// `BUZZ_CHANNEL_ID`; a UUID.
    pub channel_id: String,
    /// `BUZZ_CHANNEL`, when plain enough to show.
    pub channel_name: String,
    /// `BUZZ_THREAD_ID`; a 64-hex event id.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// `BUZZ_NPUB`.
    pub npub: String,
    /// `BUZZ_RELAY_URL`; a credential-free ws/wss URL.
    pub relay_url: String,
}

impl From<TerminalContext> for pty::TerminalContext {
    fn from(context: TerminalContext) -> Self {
        Self {
            channel_id: context.channel_id,
            channel_name: context.channel_name,
            thread_id: context.thread_id,
            npub: context.npub,
            relay_url: context.relay_url,
        }
    }
}

/// One read's output.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOutput {
    /// Raw PTY bytes, not necessarily ending on a UTF-8 boundary.
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
    /// Set once all output is read and the shell has exited.
    #[serde(default)]
    pub exited: bool,
}

/// Wakes a waiting read after input, so the shell's echo comes back at
/// once instead of after the quiet pause.
#[derive(Default)]
struct Wake {
    woken: Mutex<bool>,
    signal: Condvar,
}

impl Wake {
    fn wake(&self) {
        *self
            .woken
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = true;
        self.signal.notify_all();
    }

    /// Waits up to `pause`; true when woken.
    fn wait(&self, pause: Duration) -> bool {
        let woken = self
            .woken
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (mut woken, _) = self
            .signal
            .wait_timeout_while(woken, pause, |woken| !*woken)
            .unwrap_or_else(|poison| poison.into_inner());
        std::mem::take(&mut *woken)
    }
}

/// Session id to its owner and wake.
type Wakes = Mutex<HashMap<String, (String, Arc<Wake>)>>;

/// The host's terminals. Every [`pty::Terminals`] call may block on
/// process teardown, so all of them run on blocking threads.
#[derive(Default)]
pub struct Terminals {
    pty: pty::Terminals,
    wakes: Arc<Wakes>,
}

impl Terminals {
    /// Serves a terminal request. `asker` is the connection it came over,
    /// whose closing closes the owners it made.
    pub async fn handle(
        &self,
        request: Request,
        asker: Option<Arc<dyn Asker>>,
    ) -> Result<Response, RpcError> {
        match request {
            Request::TerminalOwner => self.create_owner(asker).await.map(Response::TerminalOwner),
            Request::TerminalSpawn {
                owner,
                context,
                cols,
                rows,
            } => self
                .spawn(owner, context, cols, rows)
                .await
                .map(Response::TerminalSession),
            Request::TerminalRead { owner, id, wait_ms } => self
                .read(owner, id, Duration::from_millis(wait_ms.into()))
                .await
                .map(Response::TerminalOutput),
            Request::TerminalWrite { owner, id, data } => {
                self.write(owner, id, data).await.map(|_| Response::Done)
            }
            Request::TerminalResize {
                owner,
                id,
                cols,
                rows,
            } => self
                .resize(owner, id, cols, rows)
                .await
                .map(|_| Response::Done),
            Request::TerminalClose { owner, id } => {
                self.close(owner, id).await.map(|_| Response::Done)
            }
            Request::TerminalCloseOwner { owner } => {
                self.close_owner(owner).await.map(|_| Response::Done)
            }
            _ => Err(failed("Not a terminal request")),
        }
    }

    pub async fn create_owner(&self, asker: Option<Arc<dyn Asker>>) -> Result<String, RpcError> {
        let terminals = self.pty.clone();
        let owner = blocking(move || terminals.create_owner()).await?;
        if let Some(asker) = asker {
            let closed = asker.closed();
            let terminals = self.pty.clone();
            let wakes = self.wakes.clone();
            let fenced = owner.clone();
            tokio::spawn(async move {
                closed.await;
                forget_owner(&wakes, &fenced);
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(error) = terminals.close_owner(&fenced) {
                        tracing::warn!(%error, "could not close a disconnected terminal owner");
                    }
                })
                .await;
            });
        }
        Ok(owner)
    }

    pub async fn spawn(
        &self,
        owner: String,
        context: TerminalContext,
        cols: u16,
        rows: u16,
    ) -> Result<String, RpcError> {
        pty::dimensions(cols, rows).map_err(failed)?;
        let terminals = self.pty.clone();
        let fenced = owner.clone();
        let id = blocking(move || terminals.spawn(&fenced, context.into(), cols, rows)).await?;
        self.wakes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(id.clone(), (owner, Arc::default()));
        Ok(id)
    }

    /// Returns as soon as there is output, or the shell has exited, or
    /// after `wait` (at most [`MAX_WAIT`]) with nothing.
    pub async fn read(
        &self,
        owner: String,
        id: String,
        wait: Duration,
    ) -> Result<TerminalOutput, RpcError> {
        let wake = self.wake(&id);
        let terminals = self.pty.clone();
        let deadline = Instant::now() + wait.min(MAX_WAIT);
        blocking(move || {
            let mut pause = BUSY_PAUSE;
            loop {
                let read = terminals.read(&owner, &id)?;
                let now = Instant::now();
                if !read.data.is_empty() || read.exited || now >= deadline {
                    return Ok(TerminalOutput {
                        data: read.data,
                        exited: read.exited,
                    });
                }
                let woken = match &wake {
                    Some(wake) => wake.wait(pause.min(deadline - now)),
                    None => {
                        std::thread::sleep(pause.min(deadline - now));
                        false
                    }
                };
                pause = if woken {
                    BUSY_PAUSE
                } else {
                    (pause * 2).min(QUIET_PAUSE)
                };
            }
        })
        .await
    }

    pub async fn write(&self, owner: String, id: String, data: Vec<u8>) -> Result<(), RpcError> {
        if data.len() > pty::MAX_PENDING_INPUT {
            return Err(failed("Terminal input exceeds 1 MiB"));
        }
        let wake = self.wake(&id);
        let terminals = self.pty.clone();
        blocking(move || terminals.write(&owner, &id, &data)).await?;
        if let Some(wake) = wake {
            wake.wake();
        }
        Ok(())
    }

    pub async fn resize(
        &self,
        owner: String,
        id: String,
        cols: u16,
        rows: u16,
    ) -> Result<(), RpcError> {
        pty::dimensions(cols, rows).map_err(failed)?;
        let terminals = self.pty.clone();
        blocking(move || terminals.resize(&owner, &id, cols, rows)).await
    }

    pub async fn close(&self, owner: String, id: String) -> Result<(), RpcError> {
        let terminals = self.pty.clone();
        let session = id.clone();
        blocking(move || terminals.close(&owner, &session)).await?;
        let wake = self
            .wakes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
        // A read waiting on the closed session returns its error now.
        if let Some((_, wake)) = wake {
            wake.wake();
        }
        Ok(())
    }

    pub async fn close_owner(&self, owner: String) -> Result<(), RpcError> {
        forget_owner(&self.wakes, &owner);
        let terminals = self.pty.clone();
        blocking(move || terminals.close_owner(&owner)).await
    }

    fn wake(&self, id: &str) -> Option<Arc<Wake>> {
        self.wakes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(id)
            .map(|(_, wake)| wake.clone())
    }
}

impl Drop for Terminals {
    /// Ends every shell with the host.
    fn drop(&mut self) {
        if let Err(error) = self.pty.shutdown() {
            tracing::warn!(%error, "terminal shutdown failed");
        }
    }
}

fn forget_owner(wakes: &Wakes, owner: &str) {
    wakes
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .retain(|_, (holder, wake)| {
            let keep = holder != owner;
            if !keep {
                wake.wake();
            }
            keep
        });
}

async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, RpcError> {
    crate::blocking(job).await.map_err(failed)
}

fn failed(message: impl Into<String>) -> RpcError {
    RpcError::Failed(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_carries_bytes_compactly_and_old_reads_wait_zero() {
        let output = TerminalOutput {
            data: vec![0xff; 100],
            exited: false,
        };
        let wire = rmp_serde::to_vec_named(&output).unwrap();
        // A byte string, not an array of integers.
        assert!(wire.len() < 120, "{} bytes", wire.len());
        assert_eq!(
            rmp_serde::from_slice::<TerminalOutput>(&wire).unwrap(),
            output
        );
        #[derive(Serialize)]
        enum Old {
            TerminalRead { owner: String, id: String },
        }
        let old = rmp_serde::to_vec_named(&Old::TerminalRead {
            owner: "o".into(),
            id: "i".into(),
        })
        .unwrap();
        match rmp_serde::from_slice::<Request>(&old).unwrap() {
            Request::TerminalRead { wait_ms, .. } => assert_eq!(wait_ms, 0),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_wake_ends_the_wait_once() {
        let wake = Wake::default();
        wake.wake();
        assert!(wake.wait(Duration::from_secs(5)));
        assert!(!wake.wait(Duration::from_millis(1)));
    }
}
