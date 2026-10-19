//! Checkpoint the workspace after all adapter processes have stopped.
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::observer::{ObserverContext, ObserverHandle};

const RECEIPT_BUDGET: usize = 2048;

/// Why a stop checkpoint failed. Helper stderr is discarded because it may
/// carry storage credentials, so these variants are the whole diagnosis.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckpointError {
    #[error("could not start buzz-agent-checkpoint: {0}")]
    Spawn(std::io::Error),
    #[error("could not read the checkpoint helper: {0}")]
    Io(std::io::Error),
    #[error("checkpoint helper did not finish within {0:?}")]
    Timeout(Duration),
    #[error("checkpoint helper failed ({0})")]
    ExitStatus(std::process::ExitStatus),
    #[error("checkpoint receipt exceeds {RECEIPT_BUDGET} bytes")]
    ReceiptOversized,
    #[error("invalid checkpoint receipt: {0}")]
    ReceiptInvalid(&'static str),
}

impl CheckpointError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Spawn(_) => "spawn",
            Self::Io(_) => "io",
            Self::Timeout(_) => "timeout",
            Self::ExitStatus(_) => "exit_status",
            Self::ReceiptOversized => "receipt_oversized",
            Self::ReceiptInvalid(_) => "receipt_invalid",
        }
    }

    fn details(&self) -> serde_json::Value {
        let mut details = serde_json::json!({"reason": self.reason()});
        if let Self::ExitStatus(status) = self {
            details["exit_code"] = status.code().into();
        }
        details
    }
}

/// Only these fields reach activity; anything else the helper adds is ignored.
#[derive(Deserialize)]
struct Receipt {
    version: u32,
    key: String,
    sha256: String,
    repositories: u64,
    bytes: u64,
}

fn receipt(bytes: &[u8]) -> Result<serde_json::Value, CheckpointError> {
    let saved: Receipt = serde_json::from_slice(bytes)
        .map_err(|_| CheckpointError::ReceiptInvalid("malformed receipt"))?;
    if saved.version != 1 {
        return Err(CheckpointError::ReceiptInvalid("unsupported version"));
    }
    if saved.sha256.len() != 64 || !saved.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CheckpointError::ReceiptInvalid("invalid sha256"));
    }
    let key = saved.key.as_str();
    if key.is_empty()
        || key.len() > 200
        || key.starts_with('/')
        || key.contains("..")
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-/._".contains(&b))
    {
        return Err(CheckpointError::ReceiptInvalid("invalid key"));
    }
    Ok(serde_json::json!({"key":saved.key,"sha256":saved.sha256,
        "repositories":saved.repositories,"bytes":saved.bytes}))
}

/// Kubernetes container termination-message path. The kubelet mounts it into
/// every container, and the in-cluster reaper reads the receipt back from the
/// agent container's terminated status (`lifecycle::CheckpointReceipt` in
/// `buzz-backend-kubernetes`).
const TERMINATION_LOG: &str = "/dev/termination-log";

/// Termination-message wire format: `{"version":1,"checkpoint":"<key>"}`.
/// Field order is the declaration order, so the bytes are stable.
#[derive(serde::Serialize)]
struct TerminationReceipt<'a> {
    version: u32,
    checkpoint: &'a str,
}

pub(crate) async fn save(observer: Option<&ObserverHandle>) -> anyhow::Result<()> {
    let mut command = Command::new("buzz-agent-checkpoint");
    command.arg("save");
    save_with_command(
        observer,
        command,
        Duration::from_secs(120),
        Path::new(TERMINATION_LOG),
    )
    .await
    .map_err(|error| {
        anyhow::Error::new(error)
            .context("checkpoint failed; retain the Sandbox workspace for explicit recovery")
    })
}

/// Record the saved checkpoint key as the container's termination message.
/// The file is never created: it exists only where the kubelet mounted it, so
/// `NotFound` is a local run and succeeds without writing. Any other open or
/// write failure is returned.
fn write_termination_receipt(path: &Path, key: &str) -> std::io::Result<()> {
    let receipt = serde_json::to_string(&TerminationReceipt {
        version: 1,
        checkpoint: key,
    })?;
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    std::io::Write::write_all(&mut file, receipt.as_bytes())
}

async fn save_with_command(
    observer: Option<&ObserverHandle>,
    mut command: Command,
    timeout: Duration,
    termination_log: &Path,
) -> Result<(), CheckpointError> {
    let emit = |kind, details| {
        if let Some(observer) = observer {
            observer.emit(kind, None, &ObserverContext::default(), details);
        }
    };
    emit("checkpoint_started", serde_json::json!({}));
    command
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    match run_checkpoint(command, timeout).await {
        Ok(details) => {
            if let Some(key) = details["key"].as_str() {
                // The checkpoint itself is saved and the stop proceeds; the
                // reaper finds no receipt and treats the checkpoint as missing.
                if let Err(error) = write_termination_receipt(termination_log, key) {
                    tracing::warn!(%error, "could not write the checkpoint termination receipt");
                    emit(
                        "checkpoint_receipt_failed",
                        serde_json::json!({"error_kind": error.kind().to_string()}),
                    );
                }
            }
            emit("checkpoint_saved", details);
            Ok(())
        }
        Err(error) => {
            tracing::error!(
                reason = error.reason(),
                "workspace checkpoint failed: {error}"
            );
            emit("checkpoint_failed", error.details());
            Err(error)
        }
    }
}

async fn run_checkpoint(
    mut command: Command,
    timeout: Duration,
) -> Result<serde_json::Value, CheckpointError> {
    let mut child = command.spawn().map_err(CheckpointError::Spawn)?;
    // `id()` is `None` once the helper is reaped. Capture the group now so a
    // failed helper's surviving descendants are still killed.
    #[cfg(unix)]
    let group = child.id().and_then(|id| i32::try_from(id).ok());
    let stdout = child.stdout.take().ok_or_else(|| {
        CheckpointError::Io(std::io::Error::other("checkpoint output is missing"))
    })?;
    let result = tokio::time::timeout(timeout, async {
        let mut output = Vec::new();
        stdout
            .take(RECEIPT_BUDGET as u64 + 1)
            .read_to_end(&mut output)
            .await
            .map_err(CheckpointError::Io)?;
        if output.len() > RECEIPT_BUDGET {
            return Err(CheckpointError::ReceiptOversized);
        }
        let status = child.wait().await.map_err(CheckpointError::Io)?;
        if !status.success() {
            return Err(CheckpointError::ExitStatus(status));
        }
        receipt(&output)
    })
    .await
    .unwrap_or(Err(CheckpointError::Timeout(timeout)));
    #[cfg(unix)]
    if let (Err(_), Some(group)) = (&result, group) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(group),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    const NO_LOG_PATH: &str = "/nonexistent/termination-log";

    fn valid_receipt() -> serde_json::Value {
        serde_json::json!({"version":1,"key":"developer/session/id.tar.gz",
            "sha256":"a".repeat(64),"repositories":2,"bytes":100})
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_receipt_is_rejected_before_the_helper_finishes() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 2049 /dev/zero; sleep 60"]);
        let started = std::time::Instant::now();
        let error = save_with_command(
            None,
            command,
            Duration::from_secs(10),
            Path::new(NO_LOG_PATH),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, CheckpointError::ReceiptOversized));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn receipt_cannot_smuggle_raw_output_into_activity() {
        assert!(receipt(valid_receipt().to_string().as_bytes()).is_ok());
        // Unknown fields from a newer helper are accepted but never projected.
        let mut extra = valid_receipt();
        extra["raw_output"] = "synthetic secret".into();
        let projected = receipt(extra.to_string().as_bytes()).unwrap();
        assert_eq!(
            projected,
            serde_json::json!({"key":"developer/session/id.tar.gz",
                "sha256":"a".repeat(64),"repositories":2,"bytes":100})
        );
        for key in ["../another-developer", "", "/etc/passwd", "key with space"] {
            let mut invalid = valid_receipt();
            invalid["key"] = key.into();
            assert!(
                matches!(
                    receipt(invalid.to_string().as_bytes()),
                    Err(CheckpointError::ReceiptInvalid("invalid key"))
                ),
                "{key:?} must be rejected"
            );
        }
        let mut future = valid_receipt();
        future["version"] = 2.into();
        assert!(matches!(
            receipt(future.to_string().as_bytes()),
            Err(CheckpointError::ReceiptInvalid("unsupported version"))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_failures_record_their_reason_and_never_a_saved_checkpoint() {
        let cases = [
            (
                "exit 3",
                serde_json::json!({"reason":"exit_status","exit_code":3}),
            ),
            (
                "printf 'not a receipt'",
                serde_json::json!({"reason":"receipt_invalid"}),
            ),
            ("sleep 60 & wait", serde_json::json!({"reason":"timeout"})),
        ];
        for (script, details) in cases {
            let observer = ObserverHandle::in_process();
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            assert!(save_with_command(
                Some(&observer),
                command,
                Duration::from_millis(100),
                Path::new(NO_LOG_PATH)
            )
            .await
            .is_err());
            let events = observer.snapshot();
            assert_eq!(
                events
                    .iter()
                    .map(|event| event.kind.as_str())
                    .collect::<Vec<_>>(),
                ["checkpoint_started", "checkpoint_failed"]
            );
            assert_eq!(events[1].payload, details, "{script}");
        }
        let observer = ObserverHandle::in_process();
        let command = Command::new("/nonexistent/checkpoint-helper");
        assert!(matches!(
            save_with_command(
                Some(&observer),
                command,
                Duration::from_secs(1),
                Path::new(NO_LOG_PATH)
            )
            .await,
            Err(CheckpointError::Spawn(_))
        ));
        let failed = observer.snapshot().last().unwrap().clone();
        assert_eq!(failed.kind, "checkpoint_failed");
        assert_eq!(failed.payload, serde_json::json!({"reason":"spawn"}));
    }

    /// The helper is already reaped when its exit status is known, so the
    /// group kill must use the PID captured at spawn to reach descendants.
    #[cfg(unix)]
    #[tokio::test]
    async fn failed_helper_descendants_are_killed_after_the_helper_exits() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("descendant.pid");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "sleep 60 >/dev/null 2>&1 & echo $! > \"$1\"; exit 1",
            "checkpoint-test",
            pid_file.to_str().unwrap(),
        ]);
        assert!(matches!(
            save_with_command(
                None,
                command,
                Duration::from_secs(5),
                Path::new(NO_LOG_PATH)
            )
            .await,
            Err(CheckpointError::ExitStatus(_))
        ));
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let pid = nix::unistd::Pid::from_raw(pid);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        // The orphan is reaped by init (or a subreaper) once killed.
        while nix::sys::signal::kill(pid, None).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "helper descendant survived a failed checkpoint"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_success_emits_verified_receipt_after_exit() {
        let observer = ObserverHandle::in_process();
        let valid = serde_json::json!({"version":1,"key":"developer/session/id.tar.gz",
            "sha256":"a".repeat(64),"repositories":2,"bytes":100})
        .to_string();
        let mut command = Command::new("sh");
        command.args(["-c", "printf '%s' \"$1\"", "checkpoint-test", &valid]);
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("termination-log");
        std::fs::write(&log, "stale").unwrap();
        save_with_command(Some(&observer), command, Duration::from_secs(1), &log)
            .await
            .unwrap();
        let events = observer.snapshot();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "checkpoint_started");
        assert_eq!(events[1].kind, "checkpoint_saved");
        assert_eq!(events[1].payload["key"], "developer/session/id.tar.gz");
        // Exact bytes: the reaper's `CheckpointReceipt` tests parse this literal.
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            r#"{"version":1,"checkpoint":"developer/session/id.tar.gz"}"#
        );
    }

    /// A mounted log that cannot be written is reported, not swallowed; the
    /// checkpoint itself still counts as saved.
    #[cfg(unix)]
    #[tokio::test]
    async fn unwritable_receipt_is_reported_without_failing_the_checkpoint() {
        let observer = ObserverHandle::in_process();
        let valid = serde_json::json!({"version":1,"key":"developer/session/id.tar.gz",
            "sha256":"a".repeat(64),"repositories":2,"bytes":100})
        .to_string();
        let mut command = Command::new("sh");
        command.args(["-c", "printf '%s' \"$1\"", "checkpoint-test", &valid]);
        // A directory exists but cannot be opened for writing.
        let dir = tempfile::tempdir().unwrap();
        save_with_command(Some(&observer), command, Duration::from_secs(1), dir.path())
            .await
            .unwrap();
        let kinds: Vec<_> = observer
            .snapshot()
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                "checkpoint_started",
                "checkpoint_receipt_failed",
                "checkpoint_saved"
            ]
        );
    }

    /// Local runs have no kubelet-mounted file; the receipt must never create
    /// one, and a failed checkpoint must never write one.
    #[cfg(unix)]
    #[tokio::test]
    async fn receipt_is_never_created_and_never_written_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("termination-log");
        let valid = serde_json::json!({"version":1,"key":"developer/session/id.tar.gz",
            "sha256":"a".repeat(64),"repositories":2,"bytes":100})
        .to_string();
        let mut command = Command::new("sh");
        command.args(["-c", "printf '%s' \"$1\"", "checkpoint-test", &valid]);
        save_with_command(None, command, Duration::from_secs(1), &absent)
            .await
            .unwrap();
        assert!(!absent.exists());
        let present = dir.path().join("present");
        std::fs::write(&present, "").unwrap();
        let mut failing = Command::new("sh");
        failing.args(["-c", "exit 1"]);
        assert!(
            save_with_command(None, failing, Duration::from_secs(1), &present)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&present).unwrap(), "");
    }
}
