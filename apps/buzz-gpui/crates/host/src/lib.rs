//! An agent host: keeps Agents' keys on this machine and runs the processes
//! and terminals their GUI asks for.
//!
//! The GUI runs each agent; the host signs as it, one bounded request at a
//! time, so its key never leaves the host, and starts the processes its
//! type declares. buzz-app's first agent system, whose controller this host
//! once ran, is gone.

pub mod api;
mod app_agents;
pub mod credentials;
mod processes;
pub mod sandbox;
pub mod terminal;

use api::{
    AppAgentRequest, HostAnswer, HostAsk, HostEvent, HostProtocol, Request, Response,
    SandboxResponse,
};
pub use buzz_agent_controller::Credentials;
use buzz_host_rpc::{EventSource, HostHandler, HostInfo, Peer, RpcError};
use futures::future::BoxFuture;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;

/// The folder under a host's state that holds its Agents, and the one it
/// had while Agents was called Agents2.
pub const AGENTS: &str = "agents";
pub const LEGACY_AGENTS: &str = "agents2";

/// Moves the Agents folder from its old name (buzz-app's
/// `migrate_agents`). Never replaces an existing folder or follows a link.
/// A GUI moves it under its instance lock before its host starts; buzzd,
/// alone on its state, when it starts.
pub fn migrate_agents(state_dir: &std::path::Path) -> Result<(), String> {
    let old = state_dir.join(LEGACY_AGENTS);
    let new = state_dir.join(AGENTS);
    let failed = |error: std::io::Error| format!("Could not migrate agent storage: {error}");
    if !real_dir(&old).map_err(failed)? {
        return Ok(());
    }
    if real_dir(&new).map_err(failed)? {
        return Err(
            "Both agents and agents2 storage exist. Resolve the conflicting folders \
                    before reopening Buzz; nothing was moved."
                .into(),
        );
    }
    std::fs::rename(&old, &new).map_err(failed)
}

/// Whether `path` is a real folder: absent is not, and a link is an error.
fn real_dir(path: &std::path::Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a link", path.display()),
        )),
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a folder", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub struct HostConfig {
    pub info: HostInfo,
    /// Holds the Agents identities and which GUI runs each.
    pub state_dir: PathBuf,
    pub credentials: Arc<dyn Credentials>,
    /// The platform data directory old Buzz keeps its agents in
    /// (`~/.local/share` on Linux), to bring them over. `None` leaves only
    /// this host's own v1 agents to bring over.
    pub legacy_dir: Option<PathBuf>,
    /// Runs remote workspaces; `None` answers that this host does not.
    pub sandbox: Option<Arc<dyn sandbox::SandboxBackend>>,
}

/// Asks the GUI on the other end of a connection.
pub trait Asker: Send + Sync {
    fn ask(&self, ask: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>>;

    /// Resolves when the GUI behind this asker disconnects; never, for a
    /// caller without a connection.
    fn closed(&self) -> BoxFuture<'static, ()> {
        Box::pin(std::future::pending())
    }
}

impl Asker for Peer<HostProtocol> {
    fn ask(&self, ask: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        let peer = self.clone();
        Box::pin(async move { peer.request(ask).await })
    }

    fn closed(&self) -> BoxFuture<'static, ()> {
        let peer = self.clone();
        Box::pin(async move { peer.closed().await })
    }
}

pub struct AgentHost {
    info: HostInfo,
    /// Never sent on: the host has no events since v1 agents went.
    events: broadcast::Sender<(u64, HostEvent)>,
    app_agents: app_agents::AppAgentHost,
    processes: processes::Processes,
    terminals: terminal::Terminals,
    sandbox: Option<Arc<dyn sandbox::SandboxBackend>>,
}

impl AgentHost {
    /// Opens the saved identities. Must run inside tokio.
    pub fn start(config: HostConfig) -> Result<Arc<Self>, String> {
        // The v1 agent controller kept its library here.
        let old = buzz_agent_controller::OldStores {
            harness: config
                .state_dir
                .join("agent-controller")
                .join("agents.json"),
            data_dir: config
                .legacy_dir
                .unwrap_or_else(|| config.state_dir.join("no-old-buzz")),
        };
        migrate_agents(&config.state_dir)?;
        let app_agents = app_agents::AppAgentHost::new(
            config.state_dir.join(AGENTS).join("identities.json"),
            old,
            config.credentials,
        );
        let (events, _) = broadcast::channel(1);
        Ok(Arc::new(Self {
            info: config.info,
            events,
            app_agents,
            processes: processes::Processes::default(),
            terminals: Default::default(),
            sandbox: config.sandbox,
        }))
    }

    /// Stops every process this host runs for agents (sign-out before Buzz
    /// restarts, and quitting).
    pub fn shutdown(&self) -> Result<(), String> {
        self.processes.stop_all();
        Ok(())
    }

    /// Serves a request that needs nothing from the GUI.
    pub async fn request(&self, request: Request) -> Result<Response, RpcError> {
        self.request_with(request, None).await
    }

    /// Serves a request; `asker` reaches the GUI that sent it.
    pub async fn request_with(
        &self,
        request: Request,
        asker: Option<Arc<dyn Asker>>,
    ) -> Result<Response, RpcError> {
        match request {
            request @ (Request::TerminalOwner
            | Request::TerminalSpawn { .. }
            | Request::TerminalRead { .. }
            | Request::TerminalWrite { .. }
            | Request::TerminalResize { .. }
            | Request::TerminalClose { .. }
            | Request::TerminalCloseOwner { .. }) => self.terminals.handle(request, asker).await,
            Request::AppAgent(request) => self
                .app_agents
                .serve(request)
                .await
                .map(Response::AppAgent)
                .map_err(RpcError::Failed),
            Request::Sandbox(request) => Ok(Response::Sandbox(match &self.sandbox {
                Some(backend) => {
                    let custody = Arc::new(self.app_agents.clone());
                    backend.serve(request, custody).await
                }
                None => SandboxResponse::unsupported(),
            })),
            // claude code: served per connection by `RpcHandler`.
            Request::ProcessSpawn { .. }
            | Request::ProcessWrite { .. }
            | Request::ProcessKill { .. } => {
                Err(RpcError::Failed("Processes need a GUI connection".into()))
            }
        }
    }
}

impl Drop for AgentHost {
    /// Stops every process this host started.
    fn drop(&mut self) {
        self.processes.stop_all();
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| "The host could not finish the operation".to_owned())?
}

/// Serves an [`AgentHost`] over host-rpc.
pub struct RpcHandler(pub Arc<AgentHost>);

impl HostHandler<HostProtocol> for RpcHandler {
    fn info(&self) -> HostInfo {
        self.0.info.clone()
    }

    fn events(&self) -> EventSource<HostProtocol> {
        self.0.events.subscribe()
    }

    fn handle(
        &self,
        peer: Peer<HostProtocol>,
        request: Request,
    ) -> BoxFuture<'static, Result<Response, RpcError>> {
        let host = self.0.clone();
        Box::pin(async move {
            if processes::is_process_request(&request) {
                return host.processes.serve(peer, request).await;
            }
            // Agents2 claims answer for this connection.
            if let Request::AppAgent(
                request @ (AppAgentRequest::Claim { .. } | AppAgentRequest::Release { .. }),
            ) = request
            {
                return host
                    .app_agents
                    .hold(peer, request)
                    .await
                    .map(Response::AppAgent)
                    .map_err(RpcError::Failed);
            }
            host.request_with(request, Some(Arc::new(peer))).await
        })
    }
}
