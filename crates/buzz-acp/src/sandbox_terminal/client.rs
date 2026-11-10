//! Bounded IPC with the contained worker; terminal bytes never enter diagnostics.

#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Result};
use buzz_sdk::sandbox_attach::Packet;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{timeout, timeout_at, Instant};
use tokio_util::codec::{FramedRead, LinesCodec};

const MAX_LINE_BYTES: usize = 32 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Full startup budget; admission must retain authority for this entire window.
pub(super) const READY_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(15);

/// The caller must stop the whole harness when worker containment is unconfirmed.
#[derive(Debug, thiserror::Error)]
#[error("terminal worker containment could not be confirmed")]
pub(super) struct ContainmentError;

/// Validated output from one local PTY worker.
pub(super) enum Event {
    /// At most 16 KiB of terminal bytes, without logging or persistence.
    Data(Vec<u8>),
    /// The worker supplied its end receipt and exited after confirmed cleanup.
    Ended { failed: bool },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Wire {
    Ready,
    Packet { packet: Packet },
    Ended { failed: bool },
}

/// One worker and its bounded pipes. Every normal exit path must call `close`.
pub(super) struct Pty {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<FramedRead<ChildStdout, LinesCodec>>,
    receipt: Option<bool>,
    exit_deadline: Option<Instant>,
    close_deadline: Option<Instant>,
    confirmed: bool,
}

impl Pty {
    /// Spawn the internal worker and require readiness within one five-second deadline.
    pub(super) async fn open(
        executable: &Path,
        workspace: &Path,
        expires_at: u64,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        Packet::Open { rows, cols }.bytes()?;
        let environment = ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"]
            .into_iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (name, value)));
        let mut command = Command::new(executable);
        // Sprig selects its multicall entry point from argv[0].
        command.as_std_mut().arg0("buzz-acp");
        let mut child = command
            .arg("sandbox-terminal-worker")
            .arg(workspace)
            .arg(expires_at.to_string())
            .env_clear()
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // SIGKILL-on-drop could interrupt the worker's descendant cleanup.
            .kill_on_drop(false)
            .spawn()
            .map_err(|_| anyhow!("could not start terminal worker"))?;
        let input = child.stdin.take();
        let output = child
            .stdout
            .take()
            .map(|pipe| FramedRead::new(pipe, LinesCodec::new_with_max_length(MAX_LINE_BYTES)));
        let mut pty = Self {
            child,
            input,
            output,
            receipt: None,
            exit_deadline: None,
            close_deadline: None,
            confirmed: false,
        };
        let started = timeout(READY_TIMEOUT, async {
            pty.write(Packet::Open { rows, cols }).await?;
            match pty.read().await? {
                Some(Wire::Ready) => Ok(()),
                Some(Wire::Ended { failed }) => {
                    pty.receipt = Some(failed);
                    bail!("terminal worker ended before readiness")
                }
                _ => bail!("terminal worker did not report readiness"),
            }
        })
        .await
        .map_err(|_| anyhow!("terminal worker readiness timed out"))
        .and_then(|result| result);
        if let Err(error) = started {
            // A setup refusal can itself mean containment is unavailable. Never
            // downgrade an unconfirmed cleanup to an ordinary startup error.
            pty.close().await?;
            return Err(error);
        }
        Ok(pty)
    }

    async fn write(&mut self, packet: Packet) -> Result<()> {
        packet.bytes()?;
        let mut bytes = serde_json::to_vec(&packet)
            .map_err(|_| anyhow!("could not encode terminal worker command"))?;
        ensure!(
            bytes.len() < MAX_LINE_BYTES,
            "terminal worker command exceeds bound"
        );
        bytes.push(b'\n');
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| anyhow!("terminal worker input is closed"))?;
        timeout(IO_TIMEOUT, async {
            input.write_all(&bytes).await?;
            input.flush().await
        })
        .await
        .map_err(|_| anyhow!("terminal worker command timed out"))?
        .map_err(|_| anyhow!("terminal worker input failed"))
    }

    async fn read(&mut self) -> Result<Option<Wire>> {
        let output = self
            .output
            .as_mut()
            .ok_or_else(|| anyhow!("terminal worker output is unavailable"))?;
        match output.next().await {
            None => Ok(None),
            Some(Ok(line)) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|_| anyhow!("invalid terminal worker output")),
            Some(Err(_)) => bail!("terminal worker output exceeds its framing bound"),
        }
    }

    /// Deliver data, resize, or close; a second open and wire acknowledgements are refused.
    pub(super) async fn send(&mut self, packet: Packet) -> Result<()> {
        ensure!(
            !self.confirmed && self.receipt.is_none(),
            "terminal worker has ended"
        );
        ensure!(
            matches!(
                packet,
                Packet::Data { .. } | Packet::Resize { .. } | Packet::Close
            ),
            "invalid terminal worker command"
        );
        self.write(packet).await
    }

    /// Read one bounded data packet or an end whose process cleanup has been confirmed.
    pub(super) async fn next(&mut self) -> Result<Event> {
        ensure!(!self.confirmed, "terminal worker has ended");
        if self.receipt.is_some() {
            // The caller may cancel next() while it awaits the final process
            // exit. Keep the receipt so the next poll can finish confirmation.
            let failed = self.confirm_bounded().await?;
            return Ok(Event::Ended { failed });
        }
        match self.read().await? {
            Some(Wire::Packet {
                packet: Packet::Data { data },
            }) => {
                let bytes = Packet::Data { data }
                    .bytes()
                    .map_err(|_| anyhow!("invalid terminal worker data"))?;
                Ok(Event::Data(bytes))
            }
            Some(Wire::Ended { failed }) => {
                self.receipt = Some(failed);
                let failed = self.confirm_bounded().await?;
                Ok(Event::Ended { failed })
            }
            None => {
                // No receipt is never a successful EOF, including signal exits.
                self.confirm_bounded().await?;
                Err(ContainmentError.into())
            }
            _ => bail!("unexpected terminal worker output"),
        }
    }

    async fn confirm_bounded(&mut self) -> Result<bool> {
        let deadline = *self
            .exit_deadline
            .get_or_insert(Instant::now() + IO_TIMEOUT);
        timeout_at(deadline, self.confirm())
            .await
            .map_err(|_| ContainmentError)?
    }

    async fn confirm(&mut self) -> Result<bool> {
        let status = self.child.wait().await.map_err(|_| ContainmentError)?;
        let Some(failed) = self.receipt else {
            return Err(ContainmentError.into());
        };
        if !matches!(status.code(), Some(0 | 1)) {
            return Err(ContainmentError.into());
        }
        self.confirmed = true;
        Ok(failed || status.code() == Some(1))
    }

    fn stop(&mut self) {
        // EOF also wakes the worker before it has installed signal handlers.
        self.input.take();
        if let Some(pid) = self.child.id().and_then(|pid| i32::try_from(pid).ok()) {
            // A live Child retains its PID until wait; after wait id() is None.
            // The explicit close path verifies the outcome even if signalling fails.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }

    /// Request cleanup, drain/discard bounded packets, and verify the end receipt and exit.
    pub(super) async fn close(&mut self) -> Result<()> {
        if self.confirmed {
            return Ok(());
        }
        self.stop();
        let deadline = *self
            .close_deadline
            .get_or_insert(Instant::now() + CLOSE_TIMEOUT);
        timeout_at(deadline, async {
            loop {
                match self.read().await {
                    Ok(Some(Wire::Ended { failed })) => self.receipt = Some(failed),
                    Ok(Some(Wire::Ready)) => {}
                    Ok(Some(Wire::Packet {
                        packet: Packet::Data { data },
                    })) => {
                        Packet::Data { data }
                            .bytes()
                            .map_err(|_| ContainmentError)?;
                    }
                    Ok(None) => break,
                    _ => return Err(ContainmentError.into()),
                }
            }
            self.confirm().await.map(|_| ())
        })
        .await
        .map_err(|_| ContainmentError)?
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if !self.confirmed {
            self.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const PEER: &str = r#"#!/usr/bin/python3
import base64,json,pathlib,signal,sys,time
root=pathlib.Path(sys.argv[2])
def end(signum=None,frame=None):
    (root/'cleaned').write_text('yes')
    print(json.dumps({'type':'ended','failed':False}),flush=True)
    sys.exit(0)
signal.signal(signal.SIGTERM,end)
packet=json.loads(sys.stdin.readline())
(root/'pid').write_text(str(__import__('os').getpid()))
if packet['rows']==47:signal.signal(signal.SIGTERM,signal.SIG_IGN)
if packet['rows']==41:
    print(json.dumps({'type':'packet','packet':{'type':'data','data':'eA=='}}),flush=True)
else:
    print(json.dumps({'type':'ready'}),flush=True)
if packet['rows']==42:
    print(json.dumps({'type':'ended','failed':False}),flush=True)
    sys.exit(1)
if packet['rows']==43:
    print(json.dumps({'type':'ended','failed':False}),flush=True)
    sys.exit(2)
if packet['rows']==44:
    print(json.dumps({'type':'ended','failed':False}),flush=True)
    while not (root/'exit-now').exists():time.sleep(.01)
    sys.exit(0)
if packet['rows']==45:
    print(json.dumps({'type':'packet','packet':{'type':'data','data':base64.b64encode(b'x'*16385).decode()}}),flush=True)
if packet['rows']==47:
    while True:time.sleep(.01)
for line in sys.stdin:
    incoming=json.loads(line)
    if incoming['type']=='data':
        print(json.dumps({'type':'packet','packet':incoming}),flush=True)
end()
"#;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let peer = root.path().join("peer");
        std::fs::write(&peer, PEER).unwrap();
        std::fs::set_permissions(&peer, std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    async fn open(root: &tempfile::TempDir, rows: u16) -> Result<Pty> {
        Pty::open(&root.path().join("peer"), root.path(), 1, rows, 80).await
    }

    #[tokio::test]
    async fn production_client_bounds_data_and_confirms_startup_cleanup() {
        let root = fixture();
        let mut pty = open(&root, 24).await.unwrap();
        pty.send(Packet::data(b"private bytes").unwrap())
            .await
            .unwrap();
        assert!(
            matches!(pty.next().await.unwrap(), Event::Data(bytes) if bytes == b"private bytes")
        );
        assert!(pty.send(Packet::Open { rows: 24, cols: 80 }).await.is_err());
        pty.close().await.unwrap();
        assert!(root.path().join("cleaned").exists());
        assert!(pty.confirmed);

        let refused = fixture();
        let error = open(&refused, 41).await.err().unwrap();
        assert!(
            error.downcast_ref::<ContainmentError>().is_none(),
            "{error}"
        );
        assert!(refused.path().join("cleaned").exists());

        let oversized = fixture();
        let mut pty = open(&oversized, 45).await.unwrap();
        let error = pty.next().await.err().unwrap();
        assert_eq!(error.to_string(), "invalid terminal worker data");
        pty.close().await.unwrap();
    }

    #[tokio::test]
    async fn process_status_controls_failure_and_containment() {
        for (rows, fatal) in [(42, false), (43, true)] {
            let root = fixture();
            let mut pty = open(&root, rows).await.unwrap();
            match pty.next().await {
                Ok(Event::Ended { failed }) => {
                    assert!(!fatal);
                    assert!(failed, "exit 1 must not become a successful end");
                }
                Err(error) => {
                    assert!(fatal);
                    assert!(error.downcast_ref::<ContainmentError>().is_some());
                }
                _ => panic!("expected end or containment failure"),
            }
        }
    }

    #[tokio::test]
    async fn cancelled_exit_confirmation_retains_receipt_and_deadline() {
        let root = fixture();
        let mut pty = open(&root, 44).await.unwrap();
        timeout(Duration::from_secs(2), async {
            while pty.receipt.is_none() {
                assert!(timeout(Duration::from_millis(25), pty.next())
                    .await
                    .is_err());
            }
        })
        .await
        .unwrap();
        assert_eq!(pty.receipt, Some(false));
        let deadline = pty.exit_deadline.unwrap();
        assert!(timeout(Duration::from_millis(25), pty.next())
            .await
            .is_err());
        assert_eq!(pty.exit_deadline, Some(deadline));
        std::fs::write(root.path().join("exit-now"), "yes").unwrap();
        assert!(matches!(
            pty.next().await.unwrap(),
            Event::Ended { failed: false }
        ));
    }

    #[tokio::test]
    async fn shutdown_timeout_is_fatal_and_does_not_sigkill() {
        let root = fixture();
        let mut pty = open(&root, 47).await.unwrap();
        let pidfd = rustix::process::pidfd_open(
            rustix::process::Pid::from_raw(pty.child.id().unwrap() as i32).unwrap(),
            rustix::process::PidfdFlags::empty(),
        )
        .unwrap();
        // The same stored deadline is reused after a cancelled close. A short
        // pre-existing deadline exercises its enforcement without a 15s test.
        pty.close_deadline = Some(Instant::now() + Duration::from_millis(100));
        let outcome = timeout(Duration::from_secs(2), pty.close()).await;
        let still_alive = pty.child.try_wait().unwrap().is_none();
        // This fixture intentionally ignores cleanup. Reap only this test peer.
        let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
        let _ = pty.child.wait().await;
        assert!(
            still_alive,
            "close must not SIGKILL instead of confirming cleanup"
        );
        let error = outcome.unwrap().unwrap_err();
        assert!(error.downcast_ref::<ContainmentError>().is_some());
    }

    #[tokio::test]
    async fn drop_requests_cleanup_without_sigkill() {
        let root = fixture();
        let pty = open(&root, 24).await.unwrap();
        let pid = nix::unistd::Pid::from_raw(pty.child.id().unwrap() as i32);
        let pidfd = rustix::process::pidfd_open(
            rustix::process::Pid::from_raw(pid.as_raw()).unwrap(),
            rustix::process::PidfdFlags::empty(),
        )
        .unwrap();
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).unwrap();
        // Observe that the peer is actually stopped before testing Drop.
        let mut stopped = false;
        for _ in 0..100 {
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw())).unwrap();
            if stat
                .rsplit_once(')')
                .unwrap()
                .1
                .trim_start()
                .starts_with('T')
            {
                stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(stopped, "peer did not stop at the scheduling barrier");
        drop(pty);
        let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::CONT);
        timeout(Duration::from_secs(2), async {
            while !root.path().join("cleaned").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
