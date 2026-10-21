use buzz_host_rpc::transport::{self, Connector, TcpConnector, TcpListener};
use buzz_host_rpc::*;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

struct Test;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum Request {
    Echo(String),
    AskClient(String),
    /// Answers with this many bytes.
    Large(usize),
}

impl Protocol for Test {
    type ClientRequest = Request;
    type ClientResponse = String;
    type HostRequest = String;
    type HostResponse = String;
    type Event = String;
}

struct Host {
    events: broadcast::Sender<(u64, String)>,
    seq: AtomicU64,
}

impl Host {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            events: broadcast::channel(capacity).0,
            seq: AtomicU64::new(0),
        })
    }

    fn emit(&self, body: &str) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.events.send((seq, body.to_string()));
    }
}

impl HostHandler<Test> for Host {
    fn info(&self) -> HostInfo {
        HostInfo {
            id: "test-host".into(),
            name: "Test".into(),
            platform: "test".into(),
            version: "0".into(),
        }
    }

    fn events(&self) -> EventSource<Test> {
        self.events.subscribe()
    }

    fn handle(
        &self,
        peer: Peer<Test>,
        request: Request,
    ) -> BoxFuture<'static, Result<String, RpcError>> {
        Box::pin(async move {
            match request {
                Request::Echo(text) => Ok(text),
                Request::AskClient(text) => peer.request(text).await,
                Request::Large(size) => Ok("x".repeat(size)),
            }
        })
    }
}

struct Signer;

impl ClientHandler<Test> for Signer {
    fn handle(&self, request: String) -> BoxFuture<'static, Result<String, RpcError>> {
        Box::pin(async move { Ok(format!("signed:{request}")) })
    }
}

fn options(token: Option<&str>) -> HostClientOptions {
    HostClientOptions {
        token: token.map(Into::into),
        ping_interval: Duration::from_millis(50),
        dead_after: Duration::from_millis(300),
        min_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(100),
        request_timeout: Duration::from_secs(5),
        ..Default::default()
    }
}

fn client(connector: Arc<dyn Connector>, token: Option<&str>) -> HostClient<Test> {
    HostClient::spawn(
        &tokio::runtime::Handle::current(),
        connector,
        Arc::new(Signer),
        options(token),
    )
}

async fn wait_for(
    state: &mut watch::Receiver<ConnectionState>,
    predicate: impl Fn(&ConnectionState) -> bool,
) -> ConnectionState {
    tokio::time::timeout(Duration::from_secs(5), state.wait_for(|s| predicate(s)))
        .await
        .expect("state change timed out")
        .expect("client dropped")
        .clone()
}

/// Waits for the next connect or disconnect, skipping host events. Unlike the
/// state watch, the event stream never coalesces transitions.
async fn next_session_change(
    events: &mut broadcast::Receiver<ClientEvent<String>>,
) -> ClientEvent<String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event, ClientEvent::Connected(_) | ClientEvent::Disconnected) {
                return event;
            }
        }
    })
    .await
    .expect("no session change")
}

fn connected(state: &ConnectionState) -> bool {
    matches!(state, ConnectionState::Connected { .. })
}

#[tokio::test]
async fn memory_request_events_and_host_requests() {
    let host = Host::new(64);
    let (connector, listener) = transport::memory();
    tokio::spawn(serve::<Test, _>(
        listener,
        host.clone(),
        ServerOptions::default(),
    ));
    let client = client(Arc::new(connector), None);
    let mut events = client.events();
    let mut state = client.state();
    let ConnectionState::Connected { host: info } = wait_for(&mut state, connected).await else {
        unreachable!()
    };
    assert_eq!(info.id, "test-host");
    assert!(matches!(
        events.recv().await.unwrap(),
        ClientEvent::Connected(_)
    ));

    assert_eq!(
        client.request(Request::Echo("hi".into())).await.unwrap(),
        "hi"
    );
    assert_eq!(
        client
            .request(Request::AskClient("event".into()))
            .await
            .unwrap(),
        "signed:event"
    );

    host.emit("one");
    host.emit("two");
    for (expected_seq, expected) in [(1, "one"), (2, "two")] {
        match events.recv().await.unwrap() {
            ClientEvent::Event { seq, body } => {
                assert_eq!((seq, body.as_str()), (expected_seq, expected));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn messages_too_large_fail_without_disconnecting() {
    let (connector, listener) = transport::memory();
    tokio::spawn(serve::<Test, _>(
        listener,
        Host::new(1),
        ServerOptions::default(),
    ));
    let client = client(Arc::new(connector), None);
    let mut events = client.events();
    let mut state = client.state();
    wait_for(&mut state, connected).await;
    next_session_change(&mut events).await;

    let large = 17 << 20;
    let failed = Err(RpcError::Failed("The message is too large to send".into()));
    assert_eq!(
        client.request(Request::Echo("x".repeat(large))).await,
        failed
    );
    assert_eq!(client.request(Request::Large(large)).await, failed);
    assert_eq!(
        client.request(Request::Echo("still".into())).await.unwrap(),
        "still"
    );
    assert!(events.try_recv().is_err(), "the connection was replaced");
}

#[tokio::test]
async fn large_frames_are_refused_before_the_handshake() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(serve::<Test, _>(
        listener,
        Host::new(1),
        ServerOptions {
            token: Some("secret".into()),
            ..Default::default()
        },
    ));
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    // A hello of 1 MiB, of which nothing is sent: the host closes at once
    // instead of waiting for it.
    stream.write_all(&(1u32 << 20).to_be_bytes()).await.unwrap();
    let mut buffer = [0; 16];
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buffer))
        .await
        .expect("the host waited for the frame");
    assert!(matches!(read, Ok(0) | Err(_)));
}

#[tokio::test]
async fn a_failed_accept_does_not_stop_the_host() {
    struct Flaky(transport::MemoryListener, bool);
    impl transport::Listener for Flaky {
        fn accept(&mut self) -> BoxFuture<'_, std::io::Result<(transport::BoxConnection, String)>> {
            if std::mem::replace(&mut self.1, false) {
                // As when the host is out of file descriptors.
                return Box::pin(async { Err(std::io::Error::other("too many open files")) });
            }
            self.0.accept()
        }
        fn describe(&self) -> String {
            self.0.describe()
        }
    }
    let (connector, listener) = transport::memory();
    let server = tokio::spawn(serve::<Test, _>(
        Flaky(listener, true),
        Host::new(1),
        ServerOptions::default(),
    ));
    let client = client(Arc::new(connector), None);
    let mut state = client.state();
    wait_for(&mut state, connected).await;
    assert_eq!(
        client.request(Request::Echo("up".into())).await.unwrap(),
        "up"
    );
    // The listener closing for good still ends it.
    drop(client);
    let ended = tokio::time::timeout(Duration::from_secs(5), server).await;
    assert!(ended.expect("the host kept serving").unwrap().is_err());
}

#[tokio::test]
async fn wrong_token_is_rejected_without_retry_loop() {
    let host = Host::new(64);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server_options = ServerOptions {
        token: Some("secret".into()),
        ..Default::default()
    };
    tokio::spawn(serve::<Test, _>(listener, host, server_options));

    let bad = client(
        Arc::new(TcpConnector {
            address: address.clone(),
        }),
        Some("wrong"),
    );
    let mut state = bad.state();
    let rejected = wait_for(&mut state, |s| {
        matches!(s, ConnectionState::Rejected { .. })
    })
    .await;
    assert_eq!(
        rejected,
        ConnectionState::Rejected {
            reason: "invalid token".into()
        }
    );
    assert_eq!(
        bad.request(Request::Echo("x".into())).await,
        Err(RpcError::NotConnected)
    );

    let missing = client(
        Arc::new(TcpConnector {
            address: address.clone(),
        }),
        None,
    );
    let mut state = missing.state();
    wait_for(&mut state, |s| {
        matches!(s, ConnectionState::Rejected { .. })
    })
    .await;

    let good = client(Arc::new(TcpConnector { address }), Some("secret"));
    let mut state = good.state();
    wait_for(&mut state, connected).await;
    assert_eq!(
        good.request(Request::Echo("ok".into())).await.unwrap(),
        "ok"
    );
}

#[tokio::test]
async fn reconnects_after_host_restart() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(serve::<Test, _>(
        listener,
        Host::new(64),
        ServerOptions::default(),
    ));

    let client = client(
        Arc::new(TcpConnector {
            address: address.clone(),
        }),
        None,
    );
    let mut state = client.state();
    let mut events = client.events();
    wait_for(&mut state, connected).await;

    server.abort();
    let _ = server.await;
    wait_for(&mut state, |s| !connected(s)).await;

    let listener = TcpListener::bind(&address).await.unwrap();
    tokio::spawn(serve::<Test, _>(
        listener,
        Host::new(64),
        ServerOptions::default(),
    ));
    wait_for(&mut state, connected).await;
    assert_eq!(
        client.request(Request::Echo("back".into())).await.unwrap(),
        "back"
    );

    let mut saw = Vec::new();
    while let Ok(event) = events.try_recv() {
        saw.push(match event {
            ClientEvent::Connected(_) => "connected",
            ClientEvent::Disconnected => "disconnected",
            _ => "other",
        });
    }
    assert_eq!(saw, ["connected", "disconnected", "connected"]);
}

#[tokio::test]
async fn a_host_that_drops_after_welcome_is_backed_off() {
    let (connector, listener) = transport::memory();
    tokio::spawn(serve::<Test, _>(
        listener,
        Host::new(1),
        // Drops every client right after welcoming it.
        ServerOptions {
            idle_timeout: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let client = client(Arc::new(connector), None);
    let mut state = client.state();
    let max = options(None).max_backoff;
    wait_for(
        &mut state,
        |state| matches!(state, ConnectionState::Disconnected { retry_in, .. } if *retry_in == max),
    )
    .await;
}

#[tokio::test]
async fn silent_host_is_detected() {
    // A proxy that forwards the handshake to a real host, then stops
    // forwarding while keeping both sockets open.
    let real = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let real_address = real.local_addr().unwrap().to_string();
    tokio::spawn(serve::<Test, _>(
        real,
        Host::new(64),
        ServerOptions::default(),
    ));
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = proxy.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (mut inbound, _) = proxy.accept().await.unwrap();
        let mut outbound = tokio::net::TcpStream::connect(real_address).await.unwrap();
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            tokio::io::copy_bidirectional(&mut inbound, &mut outbound),
        )
        .await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop((inbound, outbound, proxy));
    });

    let client = client(Arc::new(TcpConnector { address }), None);
    let mut events = client.events();
    assert!(matches!(
        next_session_change(&mut events).await,
        ClientEvent::Connected(_)
    ));
    let started = tokio::time::Instant::now();
    assert!(matches!(
        next_session_change(&mut events).await,
        ClientEvent::Disconnected
    ));
    // Detected by the dead timer, not by a closed socket.
    assert!(started.elapsed() >= Duration::from_millis(250));
}

#[tokio::test]
async fn pending_request_fails_when_connection_drops() {
    struct Hang(Arc<Host>);
    impl HostHandler<Test> for Hang {
        fn info(&self) -> HostInfo {
            self.0.info()
        }
        fn events(&self) -> EventSource<Test> {
            self.0.events()
        }
        fn handle(
            &self,
            _: Peer<Test>,
            _: Request,
        ) -> BoxFuture<'static, Result<String, RpcError>> {
            Box::pin(futures::future::pending())
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(serve::<Test, _>(
        listener,
        Arc::new(Hang(Host::new(1))),
        ServerOptions::default(),
    ));
    let client = client(Arc::new(TcpConnector { address }), None);
    let mut state = client.state();
    wait_for(&mut state, connected).await;

    let request = client.request(Request::Echo("never".into()));
    let abort = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.abort();
    };
    let (result, _) = tokio::join!(request, abort);
    assert_eq!(result, Err(RpcError::Disconnected));
}

#[tokio::test]
async fn host_request_fails_when_connection_closes() {
    struct Silent;
    impl ClientHandler<Test> for Silent {
        fn handle(&self, _: String) -> BoxFuture<'static, Result<String, RpcError>> {
            Box::pin(futures::future::pending())
        }
    }
    type Results = tokio::sync::mpsc::UnboundedSender<Result<String, RpcError>>;
    struct Asks(Arc<Host>, Results);
    impl HostHandler<Test> for Asks {
        fn info(&self) -> HostInfo {
            self.0.info()
        }
        fn events(&self) -> EventSource<Test> {
            self.0.events()
        }
        fn handle(
            &self,
            peer: Peer<Test>,
            _: Request,
        ) -> BoxFuture<'static, Result<String, RpcError>> {
            // Outlives the connection, as work a host starts for it can.
            let results = self.1.clone();
            tokio::spawn(async move {
                let _ = results.send(peer.request("sign".into()).await);
            });
            Box::pin(async { Ok(String::new()) })
        }
    }
    let (results, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (connector, listener) = transport::memory();
    tokio::spawn(serve::<Test, _>(
        listener,
        Arc::new(Asks(Host::new(1), results)),
        ServerOptions::default(),
    ));
    let client = HostClient::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(Silent),
        options(None),
    );
    let mut state = client.state();
    wait_for(&mut state, connected).await;
    client.request(Request::Echo("ask".into())).await.unwrap();
    drop(client);
    // Well before the host's 120 s request timeout.
    let result = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .expect("the request outlived its connection");
    assert_eq!(result, Some(Err(RpcError::Disconnected)));
}

#[tokio::test]
async fn lagging_client_is_told_to_resync() {
    let host = Host::new(2);
    let (connector, listener) = transport::memory();
    tokio::spawn(serve::<Test, _>(
        listener,
        host.clone(),
        ServerOptions::default(),
    ));
    let client = client(Arc::new(connector), None);
    let mut events = client.events();
    let mut state = client.state();
    wait_for(&mut state, connected).await;
    for i in 0..50 {
        host.emit(&i.to_string());
    }
    let resync = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ClientEvent::Resync = events.recv().await.unwrap() {
                return;
            }
        }
    })
    .await;
    assert!(resync.is_ok());
}

/// The same protocol a release later: one more request, host request and
/// event.
struct Newer;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum NewerRequest {
    Echo(String),
    AskClient(String),
    Shiny(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum NewerAsk {
    Attest(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum NewerEvent {
    Known(String),
    Shiny { shiny: u32 },
}

impl Protocol for Newer {
    type ClientRequest = NewerRequest;
    type ClientResponse = String;
    type HostRequest = NewerAsk;
    type HostResponse = String;
    type Event = NewerEvent;
}

struct NewerHost {
    events: broadcast::Sender<(u64, NewerEvent)>,
}

impl HostHandler<Newer> for NewerHost {
    fn info(&self) -> HostInfo {
        Host::new(1).info()
    }

    fn events(&self) -> EventSource<Newer> {
        self.events.subscribe()
    }

    fn handle(
        &self,
        peer: Peer<Newer>,
        request: NewerRequest,
    ) -> BoxFuture<'static, Result<String, RpcError>> {
        Box::pin(async move {
            match request {
                NewerRequest::AskClient(text) => peer.request(NewerAsk::Attest(text)).await,
                NewerRequest::Echo(text) | NewerRequest::Shiny(text) => Ok(text),
            }
        })
    }
}

struct NewerSigner;

impl ClientHandler<Newer> for NewerSigner {
    fn handle(&self, _: NewerAsk) -> BoxFuture<'static, Result<String, RpcError>> {
        Box::pin(async move { Ok(String::new()) })
    }
}

#[tokio::test]
async fn newer_client_requests_fail_without_disconnecting() {
    let (connector, listener) = transport::memory();
    tokio::spawn(serve::<Test, _>(
        listener,
        Host::new(64),
        ServerOptions::default(),
    ));
    let client: HostClient<Newer> = HostClient::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(NewerSigner),
        options(None),
    );
    let mut events = client.events();
    wait_for(&mut client.state(), connected).await;
    assert!(matches!(
        client.request(NewerRequest::Shiny("x".into())).await,
        Err(RpcError::Failed(_))
    ));
    assert_eq!(
        client
            .request(NewerRequest::Echo("hi".into()))
            .await
            .unwrap(),
        "hi"
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, ClientEvent::Disconnected), "{event:?}");
    }
}

#[tokio::test]
async fn newer_host_requests_and_events_do_not_disconnect() {
    let host = Arc::new(NewerHost {
        events: broadcast::channel(64).0,
    });
    let (connector, listener) = transport::memory();
    tokio::spawn(serve::<Newer, _>(
        listener,
        host.clone(),
        ServerOptions::default(),
    ));
    let client = client(Arc::new(connector), None);
    let mut events = client.events();
    wait_for(&mut client.state(), connected).await;
    assert!(matches!(
        client.request(Request::AskClient("key".into())).await,
        Err(RpcError::Failed(_))
    ));
    let _ = host.events.send((1, NewerEvent::Shiny { shiny: 1 }));
    let _ = host.events.send((2, NewerEvent::Known("known".into())));
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap() {
                ClientEvent::Connected(_) => {}
                other => return other,
            }
        }
    })
    .await
    .unwrap();
    match event {
        ClientEvent::Event { seq, body } => assert_eq!((seq, body.as_str()), (2, "known")),
        other => panic!("unexpected {other:?}"),
    }
}
