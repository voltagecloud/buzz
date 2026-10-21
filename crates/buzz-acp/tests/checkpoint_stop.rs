//! Stop the real harness binary with `--checkpoint-on-stop` and a stub
//! `buzz-agent-checkpoint` on PATH, on shutdown and startup failure paths.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

const ADAPTER: &str = r#"#!/bin/sh
echo $$ > "$PROBE_DIR/adapter.pid"
# Never answer initialize, so SIGTERM arrives while startup is still running.
[ "$ADAPTER_MODE" = hang ] && exec sleep 60
[ "$ADAPTER_MODE" = fail ] && exit 17
read -r request
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":2,"agentCapabilities":{},"authMethods":[]}}'
exec cat >/dev/null
"#;

// Records what had already happened when the harness started the checkpoint.
const HELPER: &str = r#"#!/bin/sh
test "$1" = save || exit 64
echo save >> "$PROBE_DIR/checkpoint-runs"
{
  if kill -0 "$(cat "$PROBE_DIR/adapter.pid")" 2>/dev/null; then
    echo adapter=running
  else
    echo adapter=stopped
  fi
  if grep -q '"content":"offline"' "$PROBE_DIR/relay-events" 2>/dev/null; then
    echo presence=offline
  else
    echo presence=online
  fi
} > "$PROBE_DIR/checkpoint"
test "$CHECKPOINT_FAIL" = 1 && exit 3
printf '{"version":1,"key":"developer/session/id.tar.gz","sha256":"%064d","repositories":0,"bytes":0}' 0
"#;

#[derive(Clone, Copy, PartialEq)]
enum Stop {
    /// SIGTERM after startup: the main loop drains, then checkpoints.
    AfterReady,
    /// SIGTERM during adapter initialization: shutdown cancels startup.
    DuringStartup,
    /// Initialization fails before the harness can announce readiness.
    StartupFailure,
}

struct Stopped {
    status: ExitStatus,
    /// Stub output, or `None` when the harness never ran the helper.
    checkpoint: Option<String>,
    logs: String,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_stop_checkpoints_after_drain_and_offline_presence() {
    let stopped = stop(Stop::AfterReady, false).await;
    assert_eq!(stopped.status.code(), Some(0), "{}", stopped.logs);
    assert_eq!(
        stopped.checkpoint.as_deref(),
        Some("adapter=stopped\npresence=offline\n"),
        "{}",
        stopped.logs
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_cancellation_still_checkpoints() {
    let stopped = stop(Stop::DuringStartup, false).await;
    assert_eq!(stopped.status.code(), Some(0), "{}", stopped.logs);
    assert!(stopped.checkpoint.is_some(), "{}", stopped.logs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_checkpoint_fails_either_shutdown_path() {
    for path in [Stop::AfterReady, Stop::DuringStartup] {
        let stopped = stop(path, true).await;
        assert!(stopped.checkpoint.is_some(), "{}", stopped.logs);
        assert!(!stopped.status.success(), "{}", stopped.logs);
        assert!(stopped.logs.contains("exit_status"), "{}", stopped.logs);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_failure_checkpoints_and_preserves_the_original_error() {
    for fail_checkpoint in [false, true] {
        let stopped = stop(Stop::StartupFailure, fail_checkpoint).await;
        assert!(!stopped.status.success(), "{}", stopped.logs);
        assert_eq!(
            stopped.checkpoint.as_deref(),
            Some("adapter=stopped\npresence=online\n")
        );
        assert!(
            stopped.logs.contains("all 1 agents failed to start"),
            "{}",
            stopped.logs
        );
        let records: Vec<serde_json::Value> = stopped
            .logs
            .lines()
            .filter_map(|line| {
                let envelope: serde_json::Value = serde_json::from_str(line).ok()?;
                serde_json::from_str(envelope["fields"]["record"].as_str()?).ok()
            })
            .collect();
        assert!(
            records
                .iter()
                .any(|record| record["event"] == "harness_stopped"
                    && record["details"]["reason"] == "startup_failed"),
            "{}",
            stopped.logs
        );
        let expected = if fail_checkpoint {
            "checkpoint_failed"
        } else {
            "checkpoint_saved"
        };
        assert!(
            records.iter().any(|record| record["event"] == expected),
            "{}",
            stopped.logs
        );
    }
}

async fn stop(path: Stop, fail: bool) -> Stopped {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    write_script(&dir.join("adapter"), ADAPTER);
    write_script(&bin.join("buzz-agent-checkpoint"), HELPER);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_url = format!("ws://{}", listener.local_addr().unwrap());
    let relay = tokio::spawn(fake_relay(listener, dir.join("relay-events")));

    let mut search_path = std::ffi::OsString::from(&bin);
    search_path.push(":");
    search_path.push(std::env::var_os("PATH").unwrap_or_default());
    let log = std::fs::File::create(dir.join("harness.log")).unwrap();
    let keys = nostr::Keys::generate();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_buzz-acp"))
        .env_clear()
        .env("PATH", search_path)
        .env("PROBE_DIR", dir)
        .env(
            "ADAPTER_MODE",
            match path {
                Stop::DuringStartup => "hang",
                Stop::StartupFailure => "fail",
                Stop::AfterReady => "",
            },
        )
        .env("CHECKPOINT_FAIL", if fail { "1" } else { "0" })
        .args([
            "--private-key",
            &keys.secret_key().to_secret_hex(),
            "--relay-url",
            &relay_url,
            "--agent-command",
            dir.join("adapter").to_str().unwrap(),
            "--agent-args",
            "",
            "--checkpoint-on-stop",
            "--activity-log",
        ])
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // "presence set to online" is logged in the same poll that signals
    // readiness (no await in between), so after it SIGTERM takes the
    // main-loop path rather than startup cancellation.
    let ready = || match path {
        Stop::AfterReady => std::fs::read_to_string(dir.join("harness.log"))
            .unwrap_or_default()
            .contains("presence set to online"),
        Stop::DuringStartup => dir.join("adapter.pid").exists(),
        Stop::StartupFailure => true,
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("harness never reached the stop point: {}", logs(dir)));

    if path != Stop::StartupFailure {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id().unwrap() as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
    }
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap_or_else(|_| panic!("harness did not exit: {}", logs(dir)))
        .unwrap();
    relay.abort();
    assert_eq!(
        std::fs::read_to_string(dir.join("checkpoint-runs")).unwrap_or_default(),
        "save\n",
        "the stop hook must run exactly once: {}",
        logs(dir)
    );
    Stopped {
        status,
        checkpoint: std::fs::read_to_string(dir.join("checkpoint")).ok(),
        logs: logs(dir),
    }
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn logs(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("harness.log")).unwrap_or_default()
}

/// Minimal relay: NIP-42 challenge, `OK` for every event, `EOSE` for every
/// subscription, and an empty result for every HTTP query. Published events
/// are appended to `events` before they are acknowledged.
async fn fake_relay(listener: TcpListener, events: PathBuf) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(serve(stream, events.clone()));
    }
}

async fn serve(mut stream: TcpStream, events: PathBuf) {
    let mut head = [0; 4096];
    let peeked = loop {
        let n = stream.peek(&mut head).await.unwrap_or(0);
        if n == 0 || n == head.len() || head[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            break n;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let head = String::from_utf8_lossy(&head[..peeked]).to_ascii_lowercase();
    if !head.contains("upgrade: websocket") {
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        let mut request = Vec::new();
        let mut chunk = [0; 4096];
        while request
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .is_none_or(|end| request.len() < end + 4 + length)
        {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => request.extend_from_slice(&chunk[..n]),
            }
        }
        let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                        Content-Length: 2\r\nConnection: close\r\n\r\n[]";
        let _ = stream.write_all(response.as_bytes()).await;
        return;
    }
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let _ = ws
        .send(Message::Text(r#"["AUTH","challenge"]"#.into()))
        .await;
    while let Some(Ok(message)) = ws.next().await {
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(message.to_text().unwrap_or(""))
        else {
            continue;
        };
        let reply = match frame[0].as_str() {
            Some("AUTH" | "EVENT") => {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&events)
                    .unwrap();
                std::io::Write::write_all(&mut file, format!("{}\n", frame[1]).as_bytes()).unwrap();
                serde_json::json!(["OK", frame[1]["id"], true, ""])
            }
            Some("REQ") => serde_json::json!(["EOSE", frame[1]]),
            _ => continue,
        };
        if ws
            .send(Message::Text(reply.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
}
