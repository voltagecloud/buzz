//! Parent-side producer IPC. This child receives no relay or provider secrets.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Result};
use buzz_sdk::sandbox_attach::snapshot::{Failure, MAX_BLOB_BYTES};
use futures_util::StreamExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{timeout, timeout_at, Instant};
use tokio_util::codec::{FramedRead, LinesCodec};

use super::client::ContainmentError;
use super::snapshot_worker::{Output, Receipt};

pub(super) struct Completion {
    pub(super) receipt: Option<Receipt>,
    pub(super) reason: Option<Failure>,
}

pub(super) struct Job {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<FramedRead<ChildStdout, LinesCodec>>,
    ended: Option<Completion>,
    confirmed: bool,
    exit_deadline: Option<Instant>,
    close_deadline: Option<Instant>,
}

impl Job {
    pub(super) async fn start(
        executable: &Path,
        helper: &Path,
        workspace: &Path,
        output: &Path,
        expires: u64,
        work_bytes: u64,
    ) -> Result<Self> {
        let environment = ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"]
            .into_iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (name, value)));
        let mut command = Command::new(executable);
        command.as_std_mut().arg0("buzz-acp");
        let mut child = command
            .arg("sandbox-snapshot-worker")
            .arg(workspace)
            .arg(output)
            .arg(expires.to_string())
            .arg(work_bytes.to_string())
            .arg(helper)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(false)
            .spawn()
            .map_err(|_| anyhow!("could not start snapshot worker"))?;
        let input = child.stdin.take();
        let output = child
            .stdout
            .take()
            .map(|pipe| FramedRead::new(pipe, LinesCodec::new_with_max_length(4096)));
        let mut job = Self {
            child,
            input,
            output,
            ended: None,
            confirmed: false,
            exit_deadline: None,
            close_deadline: None,
        };
        let ready = timeout(super::client::READY_TIMEOUT, job.read()).await;
        match ready {
            Ok(Ok(Some(Output::Started))) => Ok(job),
            Ok(Ok(Some(Output::Ended { receipt, reason }))) => {
                job.ended = Some(Completion { receipt, reason });
                job.close().await?;
                bail!("snapshot worker ended before readiness")
            }
            _ => {
                job.close().await?;
                bail!("snapshot worker did not confirm readiness")
            }
        }
    }

    async fn read(&mut self) -> Result<Option<Output>> {
        let output = self
            .output
            .as_mut()
            .ok_or_else(|| anyhow!("snapshot worker output unavailable"))?;
        match output.next().await {
            Some(Ok(line)) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|_| anyhow!("invalid snapshot worker receipt")),
            Some(Err(_)) => bail!("snapshot worker receipt exceeds bound"),
            None => Ok(None),
        }
    }

    async fn confirm(&mut self) -> Result<()> {
        let status = self.child.wait().await.map_err(|_| ContainmentError)?;
        let Some(completion) = self.ended.as_mut() else {
            return Err(ContainmentError.into());
        };
        if !matches!(status.code(), Some(0 | 1)) {
            return Err(ContainmentError.into());
        }
        if completion.receipt.is_some() == completion.reason.is_some() {
            return Err(ContainmentError.into());
        }
        self.confirmed = true;
        if status.code() == Some(1) {
            completion.receipt = None;
            completion.reason = Some(completion.reason.unwrap_or(Failure::Archive));
        }
        if completion.receipt.as_ref().is_some_and(|receipt| {
            receipt.version != 1
                || !receipt.scanned
                || receipt.repositories > 64
                || receipt.bytes == 0
                || receipt.bytes > MAX_BLOB_BYTES as u64
        }) {
            completion.receipt = None;
            completion.reason = Some(Failure::Archive);
        }
        Ok(())
    }

    /// Cancellation preserves the receipt and absolute exit deadline for the next poll.
    pub(super) async fn next(&mut self) -> Result<Completion> {
        ensure!(!self.confirmed, "snapshot worker already ended");
        if self.ended.is_none() {
            match self.read().await? {
                Some(Output::Ended { receipt, reason }) => {
                    self.ended = Some(Completion { receipt, reason })
                }
                _ => return Err(ContainmentError.into()),
            }
        }
        let deadline = *self
            .exit_deadline
            .get_or_insert_with(|| Instant::now() + Duration::from_secs(5));
        timeout_at(deadline, self.confirm())
            .await
            .map_err(|_| ContainmentError)??;
        self.ended.take().ok_or_else(|| ContainmentError.into())
    }

    fn stop(&mut self) {
        self.input.take();
        if let Some(pid) = self.child.id().and_then(|pid| i32::try_from(pid).ok()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }

    pub(super) async fn close(&mut self) -> Result<()> {
        if self.confirmed {
            return Ok(());
        };
        self.stop();
        let deadline = *self
            .close_deadline
            .get_or_insert_with(|| Instant::now() + Duration::from_secs(15));
        timeout_at(deadline, async {
            while let Some(output) = self.read().await.map_err(|_| ContainmentError)? {
                match output {
                    Output::Started => {}
                    Output::Ended { receipt, reason } => {
                        self.ended = Some(Completion { receipt, reason })
                    }
                }
            }
            self.confirm().await
        })
        .await
        .map_err(|_| ContainmentError)?
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if !self.confirmed {
            self.stop();
        }
    }
}
