//! A single PTY under a dedicated Linux subreaper. Relay authorization
//! belongs to its parent; this process handles bounded local PTY operations.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use buzz_sdk::sandbox_attach::{Packet, MAX_DATA_BYTES, MAX_GRANT_SECS};
use futures_util::StreamExt;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::codec::{FramedRead, LinesCodec};

use super::process_tree;

const MAX_LINE_BYTES: usize = 32 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Output<'a> {
    Ready,
    Packet { packet: &'a Packet },
    Ended { failed: bool },
}

/// This mode is intentionally internal and runs before harness configuration.
/// Exit 1 is an operation error after confirmed cleanup; exit 2 is a fatal
/// containment error, which the parent must never treat as a normal close.
pub(super) fn entry() -> i32 {
    let arguments: Vec<_> = std::env::args_os().skip(2).collect();
    let result = (|| -> Result<_> {
        ensure!(
            arguments.len() == 2,
            "terminal worker requires workspace and expiry"
        );
        let workspace = PathBuf::from(&arguments[0]).canonicalize()?;
        ensure!(workspace.is_dir(), "terminal workspace is not a directory");
        let expiry = arguments[1]
            .to_str()
            .context("invalid terminal expiry")?
            .parse::<u64>()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        ensure!(
            expiry > now && expiry - now <= MAX_GRANT_SECS,
            "invalid terminal lifetime"
        );
        process_tree::establish()?;
        Ok((workspace, Duration::from_secs(expiry - now)))
    })();
    let Ok((workspace, lifetime)) = result else {
        eprintln!("terminal worker setup refused");
        return 2;
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("terminal worker runtime unavailable");
            return 2;
        }
    };
    let result = runtime.block_on(run(workspace, lifetime));
    // Dropping the PTY wakes its readers, but only subreaper teardown proves
    // that all jobs and detached descendants have actually stopped.
    if process_tree::cleanup().is_err() {
        runtime.shutdown_timeout(Duration::from_millis(100));
        eprintln!("terminal containment failed; harness must terminate");
        return 2;
    }
    let failed = result.is_err();
    let receipt = runtime.block_on(output(&Output::Ended { failed }));
    // Tokio stdin may have an in-flight blocking read after a lease/signal
    // cancels it. Process exit retires that worker; never await it forever.
    runtime.shutdown_timeout(Duration::from_millis(100));
    if failed || receipt.is_err() {
        eprintln!("terminal worker operation failed");
        1
    } else {
        0
    }
}

async fn output(message: &Output<'_>) -> Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    ensure!(
        bytes.len() < MAX_LINE_BYTES,
        "terminal output exceeds bound"
    );
    bytes.push(b'\n');
    tokio::time::timeout(IO_TIMEOUT, async {
        let mut stdout = tokio::io::stdout();
        stdout.write_all(&bytes).await?;
        stdout.flush().await
    })
    .await
    .context("terminal output timed out")??;
    Ok(())
}

async fn run(workspace: PathBuf, lifetime: Duration) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let (mut term, mut hangup, mut interrupt) = (
        signal(SignalKind::terminate())?,
        signal(SignalKind::hangup())?,
        signal(SignalKind::interrupt())?,
    );
    let deadline = tokio::time::Instant::now() + lifetime;
    let mut lines = FramedRead::new(
        tokio::io::stdin(),
        LinesCodec::new_with_max_length(MAX_LINE_BYTES),
    );
    let first = tokio::time::timeout(IO_TIMEOUT.min(lifetime), lines.next())
        .await?
        .context("terminal open was not supplied")??;
    let Packet::Open { rows, cols } = serde_json::from_str::<Packet>(&first)? else {
        bail!("first terminal command must open the PTY");
    };
    Packet::Open { rows, cols }.bytes()?;
    let (mut pty, pts) = pty_process::open()?;
    pty.resize(pty_process::Size::new(rows, cols))?;
    let environment = ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name, value)));
    let mut child = pty_process::Command::new("/bin/sh")
        .arg("-i")
        .current_dir(workspace)
        .env_clear()
        .envs(environment)
        .env("TERM", "xterm-256color")
        .kill_on_drop(true)
        .spawn(pts)?;
    output(&Output::Ready).await?;
    let mut idle = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut buffer = [0_u8; MAX_DATA_BYTES];
    loop {
        tokio::select! {
            biased;
            _ = term.recv() => break,
            _ = hangup.recv() => break,
            _ = interrupt.recv() => break,
            _ = tick.tick() => {
                if tokio::time::Instant::now() >= deadline || idle.elapsed() >= IDLE_TIMEOUT || child.try_wait()?.is_some() { break; }
            }
            line = lines.next() => {
                let Some(line) = line else { break; };
                let packet: Packet = serde_json::from_str(&line?)?;
                let bytes = packet.bytes()?;
                match packet {
                    Packet::Data { .. } => {
                        if !bytes.is_empty() {
                            idle = tokio::time::Instant::now();
                            tokio::time::timeout(IO_TIMEOUT, pty.write_all(&bytes)).await??;
                        }
                    }
                    Packet::Resize { rows, cols } => {
                        pty.resize(pty_process::Size::new(rows, cols))?;
                        idle = tokio::time::Instant::now();
                    }
                    Packet::Close => break,
                    _ => bail!("invalid terminal worker operation"),
                }
            }
            read = pty.read(&mut buffer) => {
                match read {
                    Ok(0) => break,
                    Ok(length) => {
                        let packet = Packet::data(&buffer[..length])?;
                        output(&Output::Packet { packet: &packet }).await?;
                    }
                    // Linux PTYs report EIO when the slave closes normally.
                    Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => break,
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    drop(pty);
    Ok(())
}
