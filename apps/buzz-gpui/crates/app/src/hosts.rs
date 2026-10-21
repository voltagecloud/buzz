//! Every agent host the GUI manages: the one embedded in this process and any
//! remote `buzzd` instances from the config file.

use crate::agent_plugins::process::{HostSpawner, ProcessSinks, Spawner};
use crate::config::{Config, CredentialsBackend, HOST_TOKEN_SERVICE, RemoteHost};
use crate::runtime::Runtime;
use buzz_host::api::{HostAnswer, HostAsk, HostProtocol};
use buzz_host::credentials::{FileCredentials, KeyringCredentials};
use buzz_host::{AgentHost, HostConfig, RpcHandler};
#[cfg(unix)]
use buzz_host_rpc::transport::UnixConnector;
use buzz_host_rpc::transport::{self, Connector, TcpConnector};
use buzz_host_rpc::{
    ClientHandler, ConnectionState, HostClient, HostClientOptions, HostInfo, RpcError,
    ServerOptions,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use gpui_kit::{App, AppContext, Context, Entity, Global, SharedString, Task};
use std::sync::Arc;

/// Agent keys created on this machine use the same credential service as
/// buzz-app, so both apps see them.
pub(crate) const AGENT_KEY_SERVICE: &str = "dev.local.buzz.foundation.agents";

/// The embedded host's server, and the host itself, which sign-out asks
/// to stop its agents.
type Embedded = (tokio::task::JoinHandle<std::io::Result<()>>, Arc<AgentHost>);

pub struct Hosts {
    pub hosts: Vec<Entity<HostConnection>>,
    /// Owns the embedded host's server; dropping it stops local agents.
    _embedded: Option<Embedded>,
}

impl Global for Hosts {}

pub struct HostConnection {
    pub label: SharedString,
    /// `None` for the host inside this process.
    pub remote: Option<RemoteHost>,
    pub state: ConnectionState,
    client: Arc<HostClient<HostProtocol>>,
    /// Where output of processes started on this host goes.
    processes: ProcessSinks,
    _watch: Task<()>,
}

impl Hosts {
    pub fn init(config: &Config, cx: &mut App) {
        let runtime = Runtime::handle(cx);
        let mut hosts = Vec::new();
        let embedded = match start_embedded(config, &runtime) {
            Ok((connector, embedded)) => {
                hosts.push(HostConnection::new(
                    "This computer".into(),
                    None,
                    Arc::new(connector),
                    None,
                    cx,
                ));
                Some(embedded)
            }
            Err(error) => {
                tracing::error!(%error, "could not start the local agent host");
                None
            }
        };
        for remote in &config.hosts {
            match remote_connector(remote) {
                Ok(connector) => {
                    let token = remote_token(remote);
                    hosts.push(HostConnection::new(
                        remote.name.clone().into(),
                        Some(remote.clone()),
                        connector,
                        token,
                        cx,
                    ))
                }
                Err(error) => tracing::error!(host = %remote.name, %error, "invalid host"),
            }
        }
        cx.set_global(Self {
            hosts,
            _embedded: embedded,
        });
    }

    /// The agent host inside this process, if it started.
    pub fn embedded(cx: &App) -> Option<Arc<AgentHost>> {
        cx.try_global::<Self>()?
            ._embedded
            .as_ref()
            .map(|(_, host)| host.clone())
    }

    fn remotes(&self, cx: &App) -> Vec<RemoteHost> {
        self.hosts
            .iter()
            .filter_map(|host| host.read(cx).remote.clone())
            .collect()
    }

    /// Saves a new remote host and connects to it. The token goes to the OS
    /// keyring; without one it is kept in the config file instead, which
    /// the returned host records in its `token` field.
    pub fn add(name: &str, address: &str, token: &str, cx: &mut App) -> Result<RemoteHost, String> {
        let (name, address, token) = (name.trim(), address.trim(), token.trim());
        if name.is_empty() {
            return Err("Enter a name for this host.".into());
        }
        if token.is_empty() {
            return Err("Enter the host's token. Run `buzzd token` on that machine.".into());
        }
        let mut remote = RemoteHost {
            name: name.into(),
            address: address.into(),
            token: None,
        };
        let connector = remote_connector(&remote)?;
        let mut remotes = cx.global::<Self>().remotes(cx);
        if remotes.iter().any(|other| other.address == address) {
            return Err("That host is already added.".into());
        }
        let keyring = store_token(address, token);
        if let Err(error) = &keyring {
            tracing::warn!(
                ?error,
                "keyring unavailable; keeping the host token in the config file"
            );
            remote.token = Some(token.into());
        }
        remotes.push(remote.clone());
        if let Err(error) = crate::config::save_hosts(&remotes) {
            if keyring.is_ok() {
                let _ = buzz_keychain::delete(HOST_TOKEN_SERVICE, address);
            }
            return Err(error);
        }
        let connection = HostConnection::new(
            name.to_string().into(),
            Some(remote.clone()),
            connector,
            Some(token.into()),
            cx,
        );
        cx.global_mut::<Self>().hosts.push(connection);
        Ok(remote)
    }

    /// Disconnects from a remote host and forgets it and its token. Its
    /// agents keep running on that machine.
    pub fn remove(host: &Entity<HostConnection>, cx: &mut App) -> Result<(), String> {
        let Some(remote) = host.read(cx).remote.clone() else {
            return Err("This computer's host cannot be removed.".into());
        };
        let remotes: Vec<RemoteHost> = cx
            .global::<Self>()
            .remotes(cx)
            .into_iter()
            .filter(|other| other.address != remote.address)
            .collect();
        crate::config::save_hosts(&remotes)?;
        match buzz_keychain::delete(HOST_TOKEN_SERVICE, &remote.address) {
            Ok(()) | Err(buzz_keychain::Error::Absent) => {}
            Err(error) => tracing::warn!(?error, "could not delete the host token"),
        }
        let id = host.entity_id();
        cx.global_mut::<Self>()
            .hosts
            .retain(|host| host.entity_id() != id);
        Ok(())
    }
}

/// Replaces any token saved for this address.
fn store_token(address: &str, token: &str) -> Result<(), buzz_keychain::Error> {
    match buzz_keychain::delete(HOST_TOKEN_SERVICE, address) {
        Ok(()) | Err(buzz_keychain::Error::Absent) => {}
        Err(error) => return Err(error),
    }
    buzz_keychain::add(HOST_TOKEN_SERVICE, address, token.as_bytes())
}

fn start_embedded(
    config: &Config,
    runtime: &tokio::runtime::Handle,
) -> Result<(transport::MemoryConnector, Embedded), String> {
    let data = dirs::data_dir()
        .ok_or("No data directory")?
        .join("buzz-gpui");
    let credentials: Arc<dyn buzz_host::Credentials> = match config.embedded.credentials {
        CredentialsBackend::Keyring => Arc::new(KeyringCredentials::new(AGENT_KEY_SERVICE)),
        CredentialsBackend::Files => Arc::new(
            FileCredentials::new(data.join("agent-keys")).map_err(|error| error.to_string())?,
        ),
    };
    // The host starts background work on the I/O runtime.
    let _runtime = runtime.enter();
    let host = AgentHost::start(HostConfig {
        info: HostInfo {
            id: "local".into(),
            name: "This computer".into(),
            platform: std::env::consts::OS.into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        state_dir: data.join("host"),
        credentials,
        legacy_dir: dirs::data_dir(),
        sandbox: None,
    })?;
    let (connector, listener) = transport::memory();
    let server = runtime.spawn(buzz_host_rpc::serve(
        listener,
        Arc::new(RpcHandler(host.clone())),
        // In-process only, so there is nobody to authenticate.
        ServerOptions::default(),
    ));
    Ok((connector, (server, host)))
}

fn remote_connector(host: &RemoteHost) -> Result<Arc<dyn Connector>, String> {
    if let Some(address) = host.address.strip_prefix("tcp://")
        && !address.is_empty()
    {
        Ok(Arc::new(TcpConnector {
            address: address.into(),
        }))
    } else if let Some(path) = host.address.strip_prefix("unix://")
        && !path.is_empty()
    {
        #[cfg(unix)]
        return Ok(Arc::new(UnixConnector { path: path.into() }));
        #[cfg(not(unix))]
        return Err(format!(
            "unix://{path}: Unix sockets need macOS or Linux; use tcp://."
        ));
    } else {
        Err("Enter an address that starts with tcp:// or unix://.".into())
    }
}

/// Tokens live in the OS keyring under the host's address. A token in the
/// config file is accepted for machines without one.
fn remote_token(host: &RemoteHost) -> Option<String> {
    if let Some(token) = &host.token {
        return Some(token.clone());
    }
    buzz_keychain::read(HOST_TOKEN_SERVICE, &host.address)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
}

struct Answers {
    /// Agent type process output, by handle.
    processes: ProcessSinks,
}

impl ClientHandler<HostProtocol> for Answers {
    fn handle(&self, ask: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        match ask {
            // claude code: delivered as the ask is read, so in order.
            HostAsk::ProcessOutput { handle, output } => {
                let answer = self.processes.deliver(handle, output);
                Box::pin(async move { answer })
            }
        }
    }
}

impl HostConnection {
    fn new(
        label: SharedString,
        remote: Option<RemoteHost>,
        connector: Arc<dyn Connector>,
        token: Option<String>,
        cx: &mut App,
    ) -> Entity<Self> {
        let runtime = Runtime::handle(cx);
        let processes = ProcessSinks::default();
        let client = Arc::new(HostClient::<HostProtocol>::spawn(
            &runtime,
            connector,
            Arc::new(Answers {
                processes: processes.clone(),
            }),
            HostClientOptions {
                token,
                ..Default::default()
            },
        ));
        processes.follow(&client, &runtime);
        cx.new(|cx| {
            let watch = Self::watch(&client, &runtime, cx);
            Self {
                label,
                remote,
                state: client.state().borrow().clone(),
                client,
                processes,
                _watch: watch,
            }
        })
    }

    /// Starts agent type `owner`'s declared processes on this host.
    pub fn spawner(&self, owner: &'static str) -> Spawner {
        Arc::new(HostSpawner {
            client: self.client.clone(),
            sinks: self.processes.clone(),
            owner,
        })
    }

    /// Mirrors connection state into this entity. Older hosts' agent
    /// snapshots are of v1 agents, which this app no longer lists.
    fn watch(
        client: &Arc<HostClient<HostProtocol>>,
        runtime: &tokio::runtime::Handle,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let (sender, mut states_in) = futures::channel::mpsc::unbounded();
        let mut states = client.state();
        runtime.spawn(async move {
            while states.changed().await.is_ok() {
                let state = states.borrow_and_update().clone();
                if sender.unbounded_send(state).is_err() {
                    return;
                }
            }
        });
        cx.spawn(async move |this, cx| {
            while let Some(state) = states_in.next().await {
                let alive = this.update(cx, |this, cx| {
                    this.state = state;
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        })
    }

    pub fn reconnect(&self) {
        self.client.reconnect();
    }

    /// The RPC client, for requests made outside a GPUI context (Agents2
    /// runs). Run them on the I/O runtime.
    pub fn client(&self) -> Arc<HostClient<HostProtocol>> {
        self.client.clone()
    }
}

/// For an erase: deletes every remote host's token from the keyring and
/// removes the hosts from the config file, which can hold tokens too. No
/// keyring means no token was saved in it.
pub fn forget_remote_hosts() -> Result<(), String> {
    let remotes = Config::load().hosts;
    for remote in &remotes {
        match buzz_keychain::delete(HOST_TOKEN_SERVICE, &remote.address) {
            Ok(()) | Err(buzz_keychain::Error::Absent | buzz_keychain::Error::Unavailable) => {}
            Err(error) => return Err(format!("delete the token for {}: {error:?}", remote.name)),
        }
    }
    if remotes.is_empty() {
        return Ok(());
    }
    crate::config::save_hosts(&[])
}
