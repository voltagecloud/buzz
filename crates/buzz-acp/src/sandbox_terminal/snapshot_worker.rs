//! A fixed producer under a dedicated subreaper; no relay keys enter this child.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context, Result};
use buzz_sdk::sandbox_attach::snapshot::{Failure, MAX_BLOB_BYTES};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use super::process_tree;

const MAX_REPORT: usize = 4096;
const MAX_ERROR: usize = 32;
const MAX_REPOSITORIES: usize = 64;

/// Bounded output from the immutable scanning producer, never its diagnostics.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub(super) version: u8,
    pub(super) scanned: bool,
    pub(super) repositories: usize,
    pub(super) bytes: u64,
}

/// The final receipt is emitted only after all descendants are killed and reaped.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Output {
    Started,
    Ended {
        receipt: Option<Receipt>,
        reason: Option<Failure>,
    },
}

struct Config {
    workspace: PathBuf,
    output: PathBuf,
    helper: PathBuf,
    lifetime: Duration,
    work_bytes: u64,
}

impl Config {
    fn parse() -> Result<Self> {
        let arguments: Vec<_> = std::env::args_os().skip(2).take(6).collect();
        ensure!(
            arguments.len() == 5,
            "snapshot worker requires workspace, output, expiry, budget and fixed helper"
        );
        let workspace = PathBuf::from(&arguments[0]).canonicalize()?;
        ensure!(workspace.is_dir(), "invalid snapshot workspace");
        let raw_output = PathBuf::from(&arguments[1]);
        let parent = raw_output
            .parent()
            .context("snapshot output has no parent")?
            .canonicalize()?;
        let output = parent.join(raw_output.file_name().context("invalid snapshot output")?);
        match std::fs::symlink_metadata(&output) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => anyhow::bail!("snapshot output must be new"),
        }
        let expiry = arguments[2]
            .to_str()
            .context("invalid snapshot expiry")?
            .parse::<u64>()?;
        let lifetime = UNIX_EPOCH
            .checked_add(Duration::from_secs(expiry))
            .context("invalid snapshot lifetime")?
            .duration_since(SystemTime::now())?;
        ensure!(
            lifetime <= Duration::from_secs(3600),
            "snapshot lifetime exceeds bound"
        );
        let work_bytes = arguments[3]
            .to_str()
            .context("invalid snapshot budget")?
            .parse::<u64>()?;
        ensure!(
            (1024 * 1024..=2 * 1024 * 1024 * 1024).contains(&work_bytes),
            "snapshot work budget exceeds bounds"
        );
        let helper = PathBuf::from(&arguments[4]).canonicalize()?;
        ensure!(helper.is_file(), "snapshot producer is not a file");
        Ok(Self {
            workspace,
            output,
            helper,
            lifetime,
            work_bytes,
        })
    }
}

async fn emit(output: &Output) -> Result<()> {
    let mut bytes = serde_json::to_vec(output)?;
    ensure!(bytes.len() <= MAX_REPORT, "snapshot receipt exceeds bound");
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stdout = tokio::io::stdout();
        stdout.write_all(&bytes).await?;
        stdout.flush().await
    })
    .await??;
    Ok(())
}

async fn bounded<R: AsyncRead + Unpin>(mut source: R, limit: usize) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        let count = source.read(&mut chunk).await?;
        if count == 0 {
            return Ok(result);
        }
        ensure!(
            result.len() + count <= limit,
            "snapshot producer output exceeds bound"
        );
        result.extend_from_slice(&chunk[..count]);
    }
}

async fn run(config: &Config) -> std::result::Result<Receipt, Failure> {
    let execute = async {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let environment = ["PATH", "HOME", "LANG", "LC_ALL"]
            .into_iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (name, value)));
        let mut child = tokio::process::Command::new(&config.helper)
            .arg("--workspace")
            .arg(&config.workspace)
            .arg("--output")
            .arg(&config.output)
            .arg("--work-bytes")
            .arg(config.work_bytes.to_string())
            .current_dir(&config.workspace)
            .env_clear()
            .envs(environment)
            .env(
                "TMPDIR",
                config.output.parent().context("invalid snapshot staging")?,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Only subreaper teardown may kill jobs; dropping the direct child
            // must not bypass or interrupt cleanup of detached descendants.
            .kill_on_drop(false)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .context("snapshot producer stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("snapshot producer stderr unavailable")?;
        emit(&Output::Started).await?;
        let wait = async {
            let (stdout, stderr, status) = tokio::try_join!(
                bounded(stdout, MAX_REPORT),
                bounded(stderr, MAX_ERROR),
                async { child.wait().await.map_err(anyhow::Error::from) }
            )?;
            Ok::<_, anyhow::Error>((stdout, stderr, status))
        };
        let mut input = tokio::io::stdin();
        let mut byte = [0];
        let result = tokio::select! {
            _=terminate.recv()=>return Ok(Err(Failure::Authority)),
            _=interrupt.recv()=>return Ok(Err(Failure::Authority)),
            _=input.read(&mut byte)=>return Ok(Err(Failure::Authority)),
            result=wait=>result?,
        };
        let (stdout, stderr, status) = result;
        if !status.success() {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Refusal {
                error: String,
            }
            let reason = match serde_json::from_slice::<Refusal>(&stderr)
                .ok()
                .map(|value| value.error)
                .as_deref()
            {
                Some("scan") => Failure::Scan,
                Some("budget") => Failure::Budget,
                _ => Failure::Archive,
            };
            return Ok(Err(reason));
        }
        let receipt: Receipt = serde_json::from_slice(&stdout)?;
        ensure!(
            receipt.version == 1
                && receipt.scanned
                && receipt.repositories <= MAX_REPOSITORIES
                && receipt.bytes > 0
                && receipt.bytes <= MAX_BLOB_BYTES as u64,
            "invalid snapshot producer receipt"
        );
        Ok::<_, anyhow::Error>(Ok(receipt))
    };
    let budget = config.lifetime.min(Duration::from_secs(180));
    match tokio::time::timeout(budget, execute).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(Failure::Archive),
        Err(_) => Err(if config.lifetime <= Duration::from_secs(180) {
            Failure::Authority
        } else {
            Failure::Budget
        }),
    }
}

/// Internal local mode; the parent fixes the helper and proves relay authority.
/// Exit2 always means process containment could not be confirmed.
pub(super) fn entry() -> i32 {
    let config = match Config::parse().and_then(|config| {
        process_tree::establish()?;
        Ok(config)
    }) {
        Ok(config) => config,
        Err(_) => {
            eprintln!("snapshot worker setup refused");
            return 2;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("snapshot worker runtime unavailable");
            return 2;
        }
    };
    let mut result = runtime.block_on(run(&config));
    if process_tree::cleanup().is_err() {
        runtime.shutdown_timeout(Duration::from_millis(100));
        eprintln!("snapshot containment failed; harness must terminate");
        return 2;
    }
    result = result.and_then(|receipt| {
        let meta = std::fs::symlink_metadata(&config.output).map_err(|_| Failure::Archive)?;
        if meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.len() == receipt.bytes
            && meta.permissions().mode() & 0o077 == 0
        {
            Ok(receipt)
        } else {
            Err(Failure::Archive)
        }
    });
    if result.is_err() {
        if let Err(error) = std::fs::remove_file(&config.output) {
            if error.kind() != std::io::ErrorKind::NotFound {
                result = Err(Failure::Archive);
            }
        }
    }
    let failed = result.is_err();
    let (receipt, reason) = match result {
        Ok(receipt) => (Some(receipt), None),
        Err(reason) => (None, Some(reason)),
    };
    let sent = runtime.block_on(emit(&Output::Ended { receipt, reason }));
    runtime.shutdown_timeout(Duration::from_millis(100));
    i32::from(failed || sent.is_err())
}
