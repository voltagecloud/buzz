//! `buzzd`: an agent host for machines the GUI reaches over the network.
//!
//! It listens on localhost by default. Expose it with whatever you trust
//! (a Tailscale service, an SSH tunnel); every client must present the token.

use anyhow::{Context, Result, bail};
use buzz_host::credentials::{FileCredentials, KeyringCredentials};
use buzz_host::{AgentHost, HostConfig, RpcHandler};
use buzz_host_rpc::transport::TcpListener;
use buzz_host_rpc::{HostInfo, ServerOptions, serve};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Config file. Defaults to $XDG_CONFIG_HOME/buzzd/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the host (the default).
    Run,
    /// Print the token GUIs need to connect, creating it if needed.
    Token,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Config {
    /// Shown in the GUI. Defaults to the hostname.
    name: Option<String>,
    /// Addresses to listen on: `tcp://host:port` or `unix:///path`.
    listen: Vec<String>,
    state_dir: Option<PathBuf>,
    /// The v1 agents' working directory, gone with them; still read so
    /// older config files load.
    #[allow(dead_code)]
    workspace: Option<PathBuf>,
    /// `files` (default; owner-only key files) or `keyring`.
    credentials: Option<String>,
}

const DEFAULT_LISTEN: &str = "tcp://127.0.0.1:7420";

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let config_path = match cli.config {
        Some(path) => path,
        None => dirs::config_dir()
            .context("no config directory")?
            .join("buzzd/config.toml"),
    };
    let config = load_config(&config_path)?;
    let state_dir = match &config.state_dir {
        Some(dir) => dir.clone(),
        None => dirs::data_dir().context("no data directory")?.join("buzzd"),
    };
    buzz_keychain::private_dir(&state_dir)
        .with_context(|| format!("could not create {}", state_dir.display()))?;
    let token = token(&state_dir)?;
    match cli.command.unwrap_or(Command::Run) {
        Command::Token => {
            println!("{token}");
            Ok(())
        }
        Command::Run => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(run(config, state_dir, token))
        }
    }
}

async fn run(config: Config, state_dir: PathBuf, token: String) -> Result<()> {
    let credentials: Arc<dyn buzz_host::Credentials> =
        match config.credentials.as_deref().unwrap_or("files") {
            "files" => Arc::new(FileCredentials::new(state_dir.join("agent-keys"))?),
            "keyring" => Arc::new(KeyringCredentials::new("dev.local.buzz.foundation.agents")),
            other => bail!("unknown credentials backend {other:?}; use files or keyring"),
        };
    let name = config.name.clone().unwrap_or_else(hostname);
    let host = AgentHost::start(HostConfig {
        info: HostInfo {
            id: host_id(&state_dir)?,
            name,
            platform: std::env::consts::OS.into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        state_dir: state_dir.clone(),
        credentials,
        legacy_dir: dirs::data_dir(),
        sandbox: sandbox(&state_dir),
    })
    .map_err(anyhow::Error::msg)?;
    let handler = Arc::new(RpcHandler(host));
    let options = ServerOptions {
        token: Some(token),
        ..Default::default()
    };

    let listen = if config.listen.is_empty() {
        vec![DEFAULT_LISTEN.to_string()]
    } else {
        config.listen
    };
    let mut servers = tokio::task::JoinSet::new();
    for address in &listen {
        if let Some(address) = address.strip_prefix("tcp://") {
            let listener = TcpListener::bind(address)
                .await
                .with_context(|| format!("could not listen on tcp://{address}"))?;
            tracing::info!(address = %listener.local_addr()?, "listening");
            servers.spawn(serve(listener, handler.clone(), options.clone()));
        } else if let Some(path) = address.strip_prefix("unix://") {
            #[cfg(unix)]
            {
                let listener = buzz_host_rpc::transport::UnixListener::bind(path.into())
                    .with_context(|| format!("could not listen on unix://{path}"))?;
                tracing::info!(%path, "listening");
                servers.spawn(serve(listener, handler.clone(), options.clone()));
            }
            #[cfg(not(unix))]
            bail!("unix://{path}: Unix sockets are not supported on this platform");
        } else {
            bail!("listen address {address:?} must start with tcp:// or unix://");
        }
    }

    let terminated = terminated()?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminated => {}
        Some(result) = servers.join_next() => {
            result?.context("listener failed")?;
        }
    }
    tracing::info!("stopping agent processes");
    // Closing listeners drops their connections and with them the last
    // references to the host, whose drop stops every agent process.
    servers.shutdown().await;
    drop(handler);
    Ok(())
}

/// Remote workspaces, in builds with them.
fn sandbox(state_dir: &Path) -> Option<Arc<dyn buzz_host::sandbox::SandboxBackend>> {
    #[cfg(feature = "voltage-sandbox")]
    return Some(Arc::new(buzz_sandbox_host::Sandboxes::new(state_dir)));
    #[cfg(not(feature = "voltage-sandbox"))]
    {
        let _ = state_dir;
        None
    }
}

fn load_config(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

/// The shared secret clients must present, created once with 256 random bits.
fn token(state_dir: &Path) -> Result<String> {
    let path = state_dir.join("token");
    match std::fs::read_to_string(&path) {
        Ok(token) if !token.trim().is_empty() => return Ok(token.trim().to_string()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("could not read the token"),
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("no randomness available"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    write_private(&path, &token)?;
    Ok(token)
}

/// Stable across restarts, so the GUI can recognise the same host.
fn host_id(state_dir: &Path) -> Result<String> {
    let path = state_dir.join("host-id");
    match std::fs::read_to_string(&path) {
        Ok(id) => Ok(id.trim().to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let id = uuid::Uuid::new_v4().to_string();
            write_private(&path, &id)?;
            Ok(id)
        }
        Err(error) => Err(error).context("could not read the host id"),
    }
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    buzz_keychain::write_private(path, contents.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))
}

/// Resolves on SIGTERM, which service managers send to stop the daemon.
/// Windows has no such signal; there it resolves on Ctrl+Break, on the
/// console closing, and on logoff or shutdown.
fn terminated() -> Result<impl std::future::Future<Output = ()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        Ok(async move {
            terminate.recv().await;
        })
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown};
        let (mut pressed, mut closed) = (ctrl_break()?, ctrl_close()?);
        let (mut logoff, mut shutdown) = (ctrl_logoff()?, ctrl_shutdown()?);
        Ok(async move {
            tokio::select! {
                _ = pressed.recv() => {}
                _ = closed.recv() => {}
                _ = logoff.recv() => {}
                _ = shutdown.recv() => {}
            }
        })
    }
}

#[cfg(target_os = "linux")]
fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|name| name.trim().to_string())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Remote host".into())
}

/// macOS and Windows keep no `/etc/hostname`; the system knows the name.
/// macOS adds `.local`, which says nothing here.
#[cfg(not(target_os = "linux"))]
fn hostname() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    let name = name.strip_suffix(".local").unwrap_or(&name).trim();
    match name.is_empty() {
        true => "Remote host".into(),
        false => name.to_string(),
    }
}
