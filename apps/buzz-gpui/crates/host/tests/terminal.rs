//! Terminals through the host: the embedded host's direct path, and a
//! buzzd-style TCP server, each driving a real login shell.

use buzz_host::api::*;
use buzz_host::credentials::FileCredentials;
use buzz_host::{AgentHost, HostConfig, RpcHandler};
use buzz_host_rpc::transport::{TcpConnector, TcpListener};
use buzz_host_rpc::{
    ClientHandler, ConnectionState, HostClient, HostClientOptions, HostInfo, Protocol, RpcError,
    ServerOptions, serve, transport,
};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct NoAsks;

impl ClientHandler<HostProtocol> for NoAsks {
    fn handle(&self, _: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        Box::pin(async { Err(RpcError::Failed("unexpected ask".into())) })
    }
}

fn host(state: &tempfile::TempDir) -> Arc<AgentHost> {
    AgentHost::start(HostConfig {
        info: HostInfo {
            id: "devbox".into(),
            name: "Devbox".into(),
            platform: "linux".into(),
            version: "test".into(),
        },
        state_dir: state.path().join("state"),
        credentials: Arc::new(FileCredentials::new(state.path().join("keys")).unwrap()),
        legacy_dir: None,
    })
    .unwrap()
}

fn context() -> TerminalContext {
    TerminalContext {
        channel_id: "1b4e28ba-2fa1-11d2-883f-0016d3cca427".into(),
        channel_name: "general".into(),
        thread_id: Some("ab".repeat(32)),
        npub: format!("npub1{}", "q".repeat(58)),
        relay_url: "wss://relay.example.com".into(),
    }
}

/// Sends a request to the host either directly or over a client.
#[derive(Clone)]
enum Via {
    Direct(Arc<AgentHost>),
    Client(Arc<HostClient<HostProtocol>>),
}

impl Via {
    async fn ask(&self, request: Request) -> Result<Response, RpcError> {
        match self {
            Via::Direct(host) => host.request(request).await,
            Via::Client(client) => client.request(request).await,
        }
    }

    async fn owner(&self) -> String {
        match self.ask(Request::TerminalOwner).await.unwrap() {
            Response::TerminalOwner(owner) => owner,
            other => panic!("{other:?}"),
        }
    }

    async fn spawn(&self, owner: &str) -> String {
        match self
            .ask(Request::TerminalSpawn {
                owner: owner.into(),
                context: context(),
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap()
        {
            Response::TerminalSession(id) => id,
            other => panic!("{other:?}"),
        }
    }

    async fn write(&self, owner: &str, id: &str, text: &str) {
        let done = self
            .ask(Request::TerminalWrite {
                owner: owner.into(),
                id: id.into(),
                data: text.as_bytes().to_vec(),
            })
            .await
            .unwrap();
        assert!(matches!(done, Response::Done));
    }

    async fn read(&self, owner: &str, id: &str, wait_ms: u32) -> Result<TerminalOutput, RpcError> {
        match self
            .ask(Request::TerminalRead {
                owner: owner.into(),
                id: id.into(),
                wait_ms,
            })
            .await?
        {
            Response::TerminalOutput(output) => Ok(output),
            other => panic!("{other:?}"),
        }
    }

    /// Reads until the output contains `needle`.
    async fn read_until(&self, owner: &str, id: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            let output = self.read(owner, id, 1000).await.unwrap();
            seen.extend(output.data);
            if String::from_utf8_lossy(&seen).contains(needle) {
                return String::from_utf8_lossy(&seen).into_owned();
            }
            assert!(!output.exited, "{}", String::from_utf8_lossy(&seen));
        }
        panic!(
            "never saw {needle:?} in {:?}",
            String::from_utf8_lossy(&seen)
        );
    }
}

/// Spawns a shell, echoes, reads the Buzz context, resizes and closes.
async fn drive(via: &Via) -> String {
    let owner = via.owner().await;
    let id = via.spawn(&owner).await;
    via.write(&owner, &id, "echo \"sum:$((40+2))\"\n").await;
    via.read_until(&owner, &id, "sum:42").await;
    // A person's profile may set BUZZ_RELAY_URL itself, so it is left out.
    via.write(
        &owner,
        &id,
        "echo \"ctx:$BUZZ_CHANNEL_ID:$BUZZ_CHANNEL:$TERM:${BUZZ_THREAD_ID:0:8}:${#BUZZ_TERM_SESSION}\"\n",
    )
    .await;
    via.read_until(
        &owner,
        &id,
        "ctx:1b4e28ba-2fa1-11d2-883f-0016d3cca427:general:xterm-256color:abababab:36",
    )
    .await;
    let resized = via
        .ask(Request::TerminalResize {
            owner: owner.clone(),
            id: id.clone(),
            cols: 100,
            rows: 30,
        })
        .await
        .unwrap();
    assert!(matches!(resized, Response::Done));
    // Readline picks up the new size on SIGWINCH; other shells may not.
    if std::env::var("SHELL").is_ok_and(|shell| shell.ends_with("bash")) {
        via.write(&owner, &id, "echo \"size:${COLUMNS}x${LINES}\"\n")
            .await;
        via.read_until(&owner, &id, "size:100x30").await;
    }
    let bad = via
        .ask(Request::TerminalResize {
            owner: owner.clone(),
            id: id.clone(),
            cols: 1,
            rows: 30,
        })
        .await
        .unwrap_err();
    assert_eq!(
        bad,
        RpcError::Failed("Terminal dimensions must be 2–500 columns and 1–300 rows".into())
    );
    // Once its prompt is read, a quiet shell answers a read after the
    // wait, with nothing.
    loop {
        let started = Instant::now();
        let quiet = via.read(&owner, &id, 300).await.unwrap();
        assert!(!quiet.exited);
        if quiet.data.is_empty() {
            assert!(started.elapsed() >= Duration::from_millis(250));
            break;
        }
    }
    let closed = via
        .ask(Request::TerminalClose {
            owner: owner.clone(),
            id: id.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(closed, Response::Done));
    assert_eq!(
        via.read(&owner, &id, 0).await.unwrap_err(),
        RpcError::Failed("Terminal session is unavailable for this owner".into())
    );
    owner
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_shell_echoes_resizes_and_closes() {
    let state = tempfile::tempdir().unwrap();
    let via = Via::Direct(host(&state));
    let owner = drive(&via).await;

    // The shell's exit is reported once its output is read.
    let id = via.spawn(&owner).await;
    via.write(&owner, &id, "exit\n").await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let output = via.read(&owner, &id, 1000).await.unwrap();
        if output.exited {
            break;
        }
        assert!(Instant::now() < deadline, "the shell never exited");
    }

    // Closing the owner ends its sessions and refuses new ones.
    let id = via.spawn(&owner).await;
    let done = via
        .ask(Request::TerminalCloseOwner {
            owner: owner.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(done, Response::Done));
    assert!(via.read(&owner, &id, 0).await.is_err());
    let refused = via
        .ask(Request::TerminalSpawn {
            owner: owner.clone(),
            context: context(),
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap_err();
    assert_eq!(
        refused,
        RpcError::Failed("Terminal owner has closed".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_context_is_validated_by_the_pty_library() {
    let state = tempfile::tempdir().unwrap();
    let via = Via::Direct(host(&state));
    let owner = via.owner().await;
    let spawn = |context: TerminalContext, cols: u16| Request::TerminalSpawn {
        owner: owner.clone(),
        context,
        cols,
        rows: 24,
    };
    let refused = |message: &str| Err::<Response, _>(RpcError::Failed(message.into()));
    let mut bad = context();
    bad.channel_id = "general".into();
    assert_eq!(
        via.ask(spawn(bad, 80)).await.map(drop),
        refused("Invalid terminal channel UUID").map(drop)
    );
    let mut bad = context();
    bad.thread_id = Some("abc".into());
    assert_eq!(
        via.ask(spawn(bad, 80)).await.map(drop),
        refused("Invalid terminal thread ID").map(drop)
    );
    let mut bad = context();
    bad.npub = "npub1short".into();
    assert_eq!(
        via.ask(spawn(bad, 80)).await.map(drop),
        refused("Invalid terminal public npub").map(drop)
    );
    let mut bad = context();
    bad.relay_url = "wss://user:secret@relay.example.com".into();
    assert_eq!(
        via.ask(spawn(bad, 80)).await.map(drop),
        refused("Terminal relay URL must be a credential-free ws/wss URL").map(drop)
    );
    assert_eq!(
        via.ask(spawn(context(), 1)).await.map(drop),
        refused("Terminal dimensions must be 2–500 columns and 1–300 rows").map(drop)
    );
}

async fn connect(address: &str, token: &str) -> Arc<HostClient<HostProtocol>> {
    let client = HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(TcpConnector {
            address: address.into(),
        }),
        Arc::new(NoAsks),
        HostClientOptions {
            token: Some(token.into()),
            ..Default::default()
        },
    );
    client
        .state()
        .wait_for(|state| matches!(state, ConnectionState::Connected { .. }))
        .await
        .unwrap();
    Arc::new(client)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_shell_runs_over_tcp_and_ends_with_its_connection() {
    let state = tempfile::tempdir().unwrap();
    let host = host(&state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(serve(
        listener,
        Arc::new(RpcHandler(host.clone())),
        ServerOptions {
            token: Some("secret".into()),
            ..Default::default()
        },
    ));
    let via = Via::Client(connect(&address, "secret").await);
    drive(&via).await;

    // A shell left open when the GUI disconnects is ended with it.
    let owner = via.owner().await;
    let id = via.spawn(&owner).await;
    via.write(&owner, &id, "echo \"up:$((6*7))\"\n").await;
    via.read_until(&owner, &id, "up:42").await;
    drop(via);
    let direct = Via::Direct(host);
    let deadline = Instant::now() + Duration::from_secs(10);
    while direct.read(&owner, &id, 0).await.is_ok() {
        assert!(
            Instant::now() < deadline,
            "the shell outlived its connection"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        direct.read(&owner, &id, 0).await.unwrap_err(),
        RpcError::Failed("Terminal session is unavailable for this owner".into())
    );
}

/// A host from before terminals: its requests have no terminal variants.
struct OldHost;

#[derive(Serialize, Deserialize)]
enum OldRequest {
    Agents,
}

impl Protocol for OldHost {
    type ClientRequest = OldRequest;
    type ClientResponse = Response;
    type HostRequest = HostAsk;
    type HostResponse = HostAnswer;
    type Event = HostEvent;
}

struct OldHandler(tokio::sync::broadcast::Sender<(u64, HostEvent)>);

impl buzz_host_rpc::HostHandler<OldHost> for OldHandler {
    fn info(&self) -> HostInfo {
        HostInfo {
            id: "old".into(),
            name: "Old".into(),
            platform: "linux".into(),
            version: "0".into(),
        }
    }

    fn events(&self) -> buzz_host_rpc::EventSource<OldHost> {
        self.0.subscribe()
    }

    fn handle(
        &self,
        _: buzz_host_rpc::Peer<OldHost>,
        _: OldRequest,
    ) -> BoxFuture<'static, Result<Response, RpcError>> {
        Box::pin(async { Ok(Response::Done) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_host_says_it_cannot_run_terminals() {
    let (connector, listener) = transport::memory();
    let (events, _) = tokio::sync::broadcast::channel(4);
    tokio::spawn(serve::<OldHost, _>(
        listener,
        Arc::new(OldHandler(events)),
        ServerOptions::default(),
    ));
    let client = HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(NoAsks),
        HostClientOptions::default(),
    );
    client
        .state()
        .wait_for(|state| matches!(state, ConnectionState::Connected { .. }))
        .await
        .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        client.request(Request::TerminalOwner),
    )
    .await
    .expect("an older host must answer, not hang")
    .unwrap_err();
    assert!(error.is_unsupported(), "{error:?}");
}
