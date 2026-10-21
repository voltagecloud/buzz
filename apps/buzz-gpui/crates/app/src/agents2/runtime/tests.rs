use super::*;
use buzz_host::api::{
    HostAnswer, HostAsk, HostEvent, LaunchSpec, SANDBOX_UNSUPPORTED, SandboxSession,
};
use buzz_host_rpc::{
    ClientHandler, ConnectionState, EventSource, HostClientOptions, HostHandler, HostInfo, Peer,
    ServerOptions, serve, transport,
};
use futures::future::BoxFuture;
use tokio::sync::broadcast;

/// A host whose backend takes 90 seconds to start a session, and that
/// predates every other sandbox request.
struct SlowHost(broadcast::Sender<(u64, HostEvent)>);

impl HostHandler<HostProtocol> for SlowHost {
    fn info(&self) -> HostInfo {
        HostInfo {
            id: "devbox".into(),
            name: "Devbox".into(),
            platform: "linux".into(),
            version: "test".into(),
        }
    }

    fn events(&self) -> EventSource<HostProtocol> {
        self.0.subscribe()
    }

    fn handle(
        &self,
        _: Peer<HostProtocol>,
        request: Request,
    ) -> BoxFuture<'static, Result<Response, RpcError>> {
        Box::pin(async move {
            match request {
                Request::Sandbox(SandboxRequest::Start { .. }) => {
                    tokio::time::sleep(Duration::from_secs(90)).await;
                    Ok(Response::Sandbox(SandboxResponse::Started))
                }
                // What a host answers for a request it cannot read.
                _ => Err(RpcError::Failed(
                    "The other side does not support this request; update it.".into(),
                )),
            }
        })
    }
}

struct NoAsks;

impl ClientHandler<HostProtocol> for NoAsks {
    fn handle(&self, _: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        Box::pin(async { Err(RpcError::Failed("unexpected ask".into())) })
    }
}

#[tokio::test(start_paused = true)]
async fn a_start_may_take_longer_than_the_default_wait() {
    let (connector, listener) = transport::memory();
    let (events, _) = broadcast::channel(1);
    tokio::spawn(serve(
        listener,
        Arc::new(SlowHost(events)),
        ServerOptions::default(),
    ));
    let client = Arc::new(HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(NoAsks),
        HostClientOptions::default(),
    ));
    client
        .state()
        .wait_for(|state| matches!(state, ConnectionState::Connected { .. }))
        .await
        .unwrap();
    let start = SandboxRequest::Start {
        session: SandboxSession {
            pubkey: "a".repeat(64),
            tenant: Default::default(),
            scope: Default::default(),
            spec: LaunchSpec::default(),
        },
        replay_floor: None,
    };
    assert_eq!(
        client
            .request(Request::Sandbox(start.clone()))
            .await
            .unwrap_err(),
        RpcError::TimedOut,
        "the default wait is a minute"
    );

    let link = HostLink {
        client,
        runtime: tokio::runtime::Handle::current(),
    };
    assert!(matches!(
        link.sandbox(start).await,
        Ok(SandboxResponse::Started)
    ));
    match link.sandbox(SandboxRequest::Providers).await {
        Ok(SandboxResponse::Failed { code, .. }) => {
            assert_eq!(code.as_deref(), Some(SANDBOX_UNSUPPORTED))
        }
        other => panic!("expected an older host to be unsupported, got {other:?}"),
    }
}
