//! Peers from before v1 agents went: an older GUI's v1 requests get an
//! error answer from this host, an older host's v1 snapshot events are
//! skipped by this GUI, and Agents keeps working across both.

use buzz_host::api::*;
use buzz_host::credentials::FileCredentials;
use buzz_host::{AgentHost, HostConfig, RpcHandler};
use buzz_host_rpc::{
    ClientEvent, ClientHandler, ConnectionState, EventSource, HostClient, HostClientOptions,
    HostHandler, HostInfo, Peer, Protocol, RpcError, ServerOptions, serve, transport,
};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

fn info() -> HostInfo {
    HostInfo {
        id: "old".into(),
        name: "Old".into(),
        platform: "linux".into(),
        version: "0".into(),
    }
}

/// The parts of the protocol as GUIs and hosts with v1 agents have them.
mod old {
    use super::*;

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct AgentsSnapshot {
        pub seq: u64,
        pub agents: Vec<serde_json::Value>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub enum AgentAction {
        Start,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub enum Request {
        Agents,
        Action { id: String, action: AgentAction },
        AppAgent(AppAgentRequest),
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub enum Response {
        Agents(AgentsSnapshot),
        AppAgent(AppAgentResponse),
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub enum HostEvent {
        Agents(AgentsSnapshot),
    }

    pub struct Protocol;

    impl buzz_host_rpc::Protocol for Protocol {
        type ClientRequest = Request;
        type ClientResponse = Response;
        type HostRequest = HostAsk;
        type HostResponse = HostAnswer;
        type Event = HostEvent;
    }
}

struct NoAsks;

impl<P: Protocol<HostRequest = HostAsk, HostResponse = HostAnswer>> ClientHandler<P> for NoAsks {
    fn handle(&self, _: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        Box::pin(async { Err(RpcError::Failed("unexpected ask".into())) })
    }
}

async fn connected<P: Protocol>(client: &HostClient<P>) {
    client
        .state()
        .wait_for(|s| matches!(s, ConnectionState::Connected { .. }))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_gui_gets_an_error_for_v1_requests_and_keeps_agents() {
    let state = tempfile::tempdir().unwrap();
    let host = AgentHost::start(HostConfig {
        info: info(),
        state_dir: state.path().join("state"),
        credentials: Arc::new(FileCredentials::new(state.path().join("keys")).unwrap()),
        legacy_dir: None,
    })
    .unwrap();
    let (connector, listener) = transport::memory();
    tokio::spawn(serve(
        listener,
        Arc::new(RpcHandler(host)),
        ServerOptions::default(),
    ));
    let client = HostClient::<old::Protocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(NoAsks),
        HostClientOptions::default(),
    );
    connected(&client).await;
    for request in [
        old::Request::Agents,
        old::Request::Action {
            id: "a".into(),
            action: old::AgentAction::Start,
        },
    ] {
        let error = client.request(request).await.unwrap_err();
        assert!(error.is_unsupported(), "{error:?}");
    }
    // The connection survives, and Agents requests work as they did.
    match client
        .request(old::Request::AppAgent(AppAgentRequest::List))
        .await
        .unwrap()
    {
        old::Response::AppAgent(AppAgentResponse::Agents(agents)) => assert!(agents.is_empty()),
        other => panic!("expected the agent list, got {other:?}"),
    }
}

/// An older host: it pushes v1 snapshots and serves Agents.
struct OldHost(tokio::sync::broadcast::Sender<(u64, old::HostEvent)>);

impl HostHandler<old::Protocol> for OldHost {
    fn info(&self) -> HostInfo {
        info()
    }

    fn events(&self) -> EventSource<old::Protocol> {
        self.0.subscribe()
    }

    fn handle(
        &self,
        _: Peer<old::Protocol>,
        request: old::Request,
    ) -> BoxFuture<'static, Result<old::Response, RpcError>> {
        Box::pin(async move {
            match request {
                old::Request::AppAgent(AppAgentRequest::List) => Ok(old::Response::AppAgent(
                    AppAgentResponse::Agents(Vec::new()),
                )),
                _ => Ok(old::Response::Agents(old::AgentsSnapshot {
                    seq: 0,
                    agents: Vec::new(),
                })),
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn this_gui_skips_an_older_hosts_v1_events_and_keeps_agents() {
    let (events, _) = tokio::sync::broadcast::channel(8);
    let (connector, listener) = transport::memory();
    tokio::spawn(serve(
        listener,
        Arc::new(OldHost(events.clone())),
        ServerOptions::default(),
    ));
    let client = HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(NoAsks),
        HostClientOptions::default(),
    );
    let mut seen = client.events();
    connected(&client).await;
    let snapshot = old::AgentsSnapshot {
        seq: 1,
        agents: vec![serde_json::json!({ "id": "a", "status": "running" })],
    };
    events.send((1, old::HostEvent::Agents(snapshot))).unwrap();
    // Agents requests still work after the event went by.
    match client
        .request(Request::AppAgent(AppAgentRequest::List))
        .await
        .unwrap()
    {
        Response::AppAgent(AppAgentResponse::Agents(agents)) => assert!(agents.is_empty()),
        other => panic!("expected the agent list, got {other:?}"),
    }
    // Nothing arrived but the connection.
    let quiet = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            match seen.recv().await.unwrap() {
                ClientEvent::Connected(_) => {}
                other => return other,
            }
        }
    })
    .await;
    assert!(quiet.is_err(), "{quiet:?}");
    assert!(matches!(
        *client.state().borrow(),
        ConnectionState::Connected { .. }
    ));
}
