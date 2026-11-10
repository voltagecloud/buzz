#![cfg(target_os = "linux")]

//! Drive the production binary and a real controlling PTY. Descendant cleanup
//! is checked independently through /proc, including double-fork + setsid.

use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use buzz_sdk::sandbox_attach::Packet;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_util::codec::{FramedRead, LinesCodec};

const BOUND: Duration = Duration::from_secs(8);

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: FramedRead<ChildStdout, LinesCodec>,
}

impl Worker {
    fn start(workspace: &std::path::Path, seconds: u64) -> Self {
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + seconds;
        let mut child = Command::new(env!("CARGO_BIN_EXE_buzz-acp"))
            .args([
                "sandbox-terminal-worker",
                workspace.to_str().unwrap(),
                &expiry.to_string(),
            ])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", workspace)
            .env("BUZZ_PRIVATE_KEY", "must-not-reach-shell")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take(),
            output: FramedRead::new(
                child.stdout.take().unwrap(),
                LinesCodec::new_with_max_length(32768),
            ),
            child,
        }
    }

    async fn send(&mut self, packet: Packet) {
        let mut bytes = serde_json::to_vec(&packet).unwrap();
        bytes.push(b'\n');
        self.input
            .as_mut()
            .unwrap()
            .write_all(&bytes)
            .await
            .unwrap();
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(BOUND, self.output.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn open(&mut self) {
        self.send(Packet::Open { rows: 24, cols: 80 }).await;
        assert_eq!(self.next().await, json!({"type":"ready"}));
    }

    async fn until(&mut self, marker: &str) -> String {
        let mut text = String::new();
        while !text.contains(marker) {
            assert!(text.len() < 65536, "terminal test output exceeded bound");
            let next = self.next().await;
            assert_eq!(next["type"], "packet");
            let packet: Packet = serde_json::from_value(next["packet"].clone()).unwrap();
            text.push_str(&String::from_utf8_lossy(&packet.bytes().unwrap()));
        }
        text
    }

    async fn close(&mut self) {
        self.input.take();
        let status = tokio::time::timeout(BOUND, self.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "worker failed after EOF: {status}");
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            let deadline = std::time::Instant::now() + BOUND;
            while std::time::Instant::now() < deadline {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.child.start_kill();
        }
    }
}

#[tokio::test]
async fn terminal_worker_has_real_pty_resize_and_a_clean_environment() {
    let workspace = tempfile::tempdir().unwrap();
    let mut worker = Worker::start(workspace.path(), 60);
    worker.open().await;
    worker.send(Packet::Resize { rows: 37, cols: 91 }).await;
    worker.send(Packet::data(b"stty size; printf '%s%s\\n' TTY_ OK; test -z \"$BUZZ_PRIVATE_KEY\" && printf '%s%s\\n' CLEAN_ ENV\n").unwrap()).await;
    let output = worker.until("CLEAN_ENV").await;
    assert!(output.contains("37 91"));
    assert!(output.contains("TTY_OK"));
    assert!(!output.contains("must-not-reach-shell"));
    worker.close().await;
}

struct Detached(i32, String);
impl Drop for Detached {
    fn drop(&mut self) {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.0)).ok();
        if stat
            .as_deref()
            .and_then(|stat| stat.rsplit_once(')'))
            .and_then(|(_, stat)| stat.split_whitespace().nth(19))
            == Some(self.1.as_str())
        {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.0),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

#[tokio::test]
async fn terminal_worker_reaps_setsid_double_fork_children_and_preserves_siblings() {
    let workspace = tempfile::tempdir().unwrap();
    let pid_file = workspace.path().join("detached.pid");
    let mut sibling = Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut worker = Worker::start(workspace.path(), 60);
    worker.open().await;
    let script = r#"import os,time; p=os.fork(); p and os._exit(0); os.setsid(); p=os.fork(); p and os._exit(0); open("detached.pid","w").write(str(os.getpid())); time.sleep(60)"#;
    worker
        .send(Packet::data(format!("python3 -c '{script}'\n").as_bytes()).unwrap())
        .await;
    let pid = tokio::time::timeout(BOUND, async {
        loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|value| value.parse::<i32>().ok())
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let started = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_owned();
    let _cleanup = Detached(pid, started);
    worker.close().await;
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "detached terminal process survived"
    );
    assert!(
        sibling.try_wait().unwrap().is_none(),
        "harness sibling was killed"
    );
    sibling.kill().await.unwrap();
    sibling.wait().await.unwrap();
}

#[tokio::test]
async fn terminal_worker_expiry_ends_a_live_shell_without_parent_eof() {
    let workspace = tempfile::tempdir().unwrap();
    let mut worker = Worker::start(workspace.path(), 2);
    worker.open().await;
    assert!(worker.input.is_some());
    let status = tokio::time::timeout(Duration::from_secs(4), worker.child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn terminal_worker_rejects_invalid_dimensions_and_bounded_input() {
    let workspace = tempfile::tempdir().unwrap();
    let mut invalid = Worker::start(workspace.path(), 60);
    invalid.send(Packet::Open { rows: 0, cols: 80 }).await;
    assert_eq!(
        tokio::time::timeout(BOUND, invalid.child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(1)
    );
    let mut worker = Worker::start(workspace.path(), 60);
    worker.open().await;
    let mut oversized = serde_json::to_vec(&Packet::Resize { rows: 24, cols: 80 }).unwrap();
    oversized.extend_from_slice(&vec![b' '; 32769]);
    oversized.push(b'\n');
    worker
        .input
        .as_mut()
        .unwrap()
        .write_all(&oversized)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(BOUND, worker.child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(1)
    );
}
