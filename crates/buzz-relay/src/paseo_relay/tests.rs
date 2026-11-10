//! The Paseo relay protocol over real WebSockets (a local axum server running
//! the production upgrade path, with tungstenite sockets as daemon and app),
//! and the hub's fencing and bounds driven directly.

use super::*;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as Ws;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn community(n: u128) -> CommunityId {
    CommunityId::from_uuid(uuid::Uuid::from_u128(n))
}

fn fast_nudge() -> Limits {
    Limits {
        nudge: [Duration::from_millis(100); 3],
        ..Limits::default()
    }
}

/// Serve `/ws` for one community on a local port.
async fn serve_relay(hub: Arc<Hub>) -> String {
    let app = Router::new().route(
        "/ws",
        get(move |Query(params): Query<Params>, ws: WebSocketUpgrade| {
            let hub = Arc::clone(&hub);
            async move { upgrade(hub, community(1), params, ws) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("ws://{addr}/ws")
}

async fn open(base: &str, query: &str) -> Socket {
    connect_async(format!("{base}?{query}")).await.unwrap().0
}

/// The next data or close frame within `wait`, skipping keepalives.
async fn next_within(socket: &mut Socket, wait: Duration) -> Option<Ws> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let frame = tokio::time::timeout_at(deadline, socket.next())
            .await
            .ok()?;
        let frame = frame.expect("socket ended").expect("socket failed");
        if !matches!(frame, Ws::Ping(_) | Ws::Pong(_)) {
            return Some(frame);
        }
    }
}

async fn next(socket: &mut Socket) -> Ws {
    next_within(socket, Duration::from_secs(5))
        .await
        .expect("relay sent nothing within 5s")
}

async fn next_json(socket: &mut Socket) -> Value {
    match next(socket).await {
        Ws::Text(body) => serde_json::from_str(&body).unwrap(),
        other => panic!("expected a JSON control message, got {other:?}"),
    }
}

async fn closed_with(socket: &mut Socket) -> u16 {
    match next(socket).await {
        Ws::Close(Some(frame)) => frame.code.into(),
        other => panic!("expected a close frame, got {other:?}"),
    }
}

/// Take everything queued for a registered socket, releasing its bytes as
/// the socket's writer would after sending.
fn take(registered: &mut Registered) -> Vec<Outbound> {
    let taken: Vec<Outbound> = std::iter::from_fn(|| registered.rx.try_recv().ok()).collect();
    for next in &taken {
        registered.release(next);
    }
    taken
}

fn closes(outbound: &[Outbound]) -> Vec<u16> {
    outbound
        .iter()
        .filter_map(|o| match o {
            Outbound::Close(code, _) => Some(*code),
            Outbound::Frame(_) => None,
        })
        .collect()
}

fn notices(outbound: &[Outbound]) -> Vec<Value> {
    outbound
        .iter()
        .filter_map(|o| match o {
            Outbound::Frame(Message::Text(body)) => serde_json::from_str(body.as_str()).ok(),
            _ => None,
        })
        .collect()
}

/// A daemon and two app sockets through every protocol step, in the order a
/// paired phone produces them.
#[tokio::test]
async fn a_daemon_and_its_clients_route_through_the_relay() {
    let hub = Arc::new(Hub::default());
    let base = serve_relay(Arc::clone(&hub)).await;

    // The daemon's control socket learns of no clients yet.
    let mut control = open(&base, "serverId=srv_a&role=server&v=2").await;
    assert_eq!(
        next_json(&mut control).await,
        json!({"type":"sync","connectionIds":[]})
    );

    // An app connects without a connection id; the relay assigns one and
    // tells the daemon.
    let mut app = open(&base, "serverId=srv_a&role=client&v=2").await;
    let connected = next_json(&mut control).await;
    assert_eq!(connected["type"], "connected");
    let connection = connected["connectionId"].as_str().unwrap().to_owned();
    assert!(
        connection.starts_with("conn_") && connection.len() == 21,
        "{connection}"
    );

    // Frames sent before the daemon's data socket exists are buffered and
    // flushed in order, text and binary alike.
    app.send(Ws::text("hello")).await.unwrap();
    app.send(Ws::binary(vec![1u8, 2, 3])).await.unwrap();
    let data_query = format!("serverId=srv_a&role=server&v=2&connectionId={connection}");
    let mut data = open(&base, &data_query).await;
    assert_eq!(next(&mut data).await, Ws::text("hello"));
    assert_eq!(next(&mut data).await, Ws::binary(vec![1u8, 2, 3]));

    // Frames then copy both ways, to every app socket sharing the id.
    let tab_query = format!("serverId=srv_a&role=client&v=2&connectionId={connection}");
    let mut tab = open(&base, &tab_query).await;
    assert_eq!(next_json(&mut control).await["type"], "connected");
    data.send(Ws::text("reply")).await.unwrap();
    assert_eq!(next(&mut app).await, Ws::text("reply"));
    assert_eq!(next(&mut tab).await, Ws::text("reply"));
    tab.send(Ws::text("from tab")).await.unwrap();
    assert_eq!(next(&mut data).await, Ws::text("from tab"));

    // A new control socket replaces the old one and receives the live list.
    let mut control2 = open(&base, "serverId=srv_a&role=server&v=2").await;
    assert_eq!(closed_with(&mut control).await, close::POLICY);
    assert_eq!(
        next_json(&mut control2).await,
        json!({"type":"sync","connectionIds":[connection.clone()]})
    );

    // Older daemons keep the control socket alive with JSON pings. A large
    // frame is never parsed, so only the small ping is answered.
    let padded = format!(
        r#"{{"type":"ping","pad":"{}"}}"#,
        "x".repeat(MAX_PING_BYTES)
    );
    control2.send(Ws::text(padded)).await.unwrap();
    control2.send(Ws::text(r#"{"type":"ping"}"#)).await.unwrap();
    assert_eq!(next_json(&mut control2).await["type"], "pong");
    assert!(next_within(&mut control2, Duration::from_millis(200))
        .await
        .is_none());

    // When the last app socket closes, the daemon's data socket is released
    // and the control socket is told.
    app.close(None).await.unwrap();
    tab.close(None).await.unwrap();
    assert_eq!(closed_with(&mut data).await, close::GOING_AWAY);
    assert_eq!(
        next_json(&mut control2).await,
        json!({"type":"disconnected","connectionId":connection})
    );

    // A data socket for a connection with no app is refused.
    let mut stray = open(
        &base,
        "serverId=srv_a&role=server&v=2&connectionId=conn_none",
    )
    .await;
    assert_eq!(closed_with(&mut stray).await, close::GOING_AWAY);

    // When the daemon's data socket drops, its app must reconnect.
    let mut phone = open(
        &base,
        "serverId=srv_a&role=client&v=2&connectionId=conn_phone",
    )
    .await;
    assert_eq!(next_json(&mut control2).await["connectionId"], "conn_phone");
    let mut phone_data = open(
        &base,
        "serverId=srv_a&role=server&v=2&connectionId=conn_phone",
    )
    .await;
    phone_data.close(None).await.unwrap();
    assert_eq!(closed_with(&mut phone).await, close::RESTART);

    // Nothing is left behind once every socket is gone: no session, and no
    // byte of buffered or queued frames.
    drop(phone);
    control2.close(None).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while hub.server_count() != 0 || hub.bytes_in_use() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session or bytes were not released"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A client whose daemon never opens the data socket gets a resent `sync`,
/// then the daemon's control socket is reset, then the client is closed so
/// that it does not wait (and buffer) indefinitely.
#[tokio::test]
async fn an_unanswered_client_is_nudged_then_reset_then_closed() {
    let base = serve_relay(Arc::new(Hub::with_limits(fast_nudge()))).await;
    let mut control = open(&base, "serverId=srv_b&role=server&v=2").await;
    assert_eq!(next_json(&mut control).await["type"], "sync");
    let mut app = open(&base, "serverId=srv_b&role=client&v=2&connectionId=conn_x").await;
    assert_eq!(next_json(&mut control).await["type"], "connected");
    assert_eq!(
        next_json(&mut control).await,
        json!({"type":"sync","connectionIds":["conn_x"]})
    );
    assert_eq!(closed_with(&mut control).await, close::INTERNAL);
    assert_eq!(closed_with(&mut app).await, close::TRY_AGAIN);
}

/// Once the daemon opens the data socket, the nudge does nothing.
#[tokio::test]
async fn an_answered_client_is_never_nudged() {
    let base = serve_relay(Arc::new(Hub::with_limits(fast_nudge()))).await;
    let mut control = open(&base, "serverId=srv_c&role=server&v=2").await;
    assert_eq!(next_json(&mut control).await["type"], "sync");
    let mut app = open(&base, "serverId=srv_c&role=client&v=2&connectionId=conn_y").await;
    assert_eq!(next_json(&mut control).await["type"], "connected");
    let _data = open(&base, "serverId=srv_c&role=server&v=2&connectionId=conn_y").await;
    assert!(next_within(&mut control, Duration::from_millis(500))
        .await
        .is_none());
    assert!(next_within(&mut app, Duration::from_millis(100))
        .await
        .is_none());
}

#[tokio::test]
async fn malformed_requests_are_refused_before_the_upgrade() {
    let base = serve_relay(Arc::new(Hub::default())).await;
    let too_long = format!("serverId={}&role=server&v=2", "s".repeat(MAX_ID_LEN + 1));
    for query in [
        "serverId=s&v=2",
        "serverId=s&role=admin&v=2",
        "role=server&v=2",
        "serverId=s&role=server",
        "serverId=s&role=server&v=1",
        "serverId=s&role=server&v=3",
        "serverId=s%2F..&role=server&v=2",
        "serverId=s&role=client&v=2&connectionId=a%20b",
        &too_long,
    ] {
        match connect_async(format!("{base}?{query}")).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), 400, "{query}");
            }
            other => panic!("{query}: expected HTTP 400, got {:?}", other.map(|_| ())),
        }
    }
}

/// The same `serverId` in two communities is two sessions.
#[tokio::test]
async fn sessions_never_cross_communities() {
    let hub = Arc::new(Hub::default());
    let mut control_a = hub
        .register(&(community(1), "srv".into()), &Role::Control)
        .unwrap();
    assert_eq!(notices(&take(&mut control_a))[0]["type"], "sync");
    let _client_b = hub
        .register(
            &(community(2), "srv".into()),
            &Role::Client("conn_b".into()),
        )
        .unwrap();
    assert!(
        take(&mut control_a).is_empty(),
        "community 1 saw community 2's client"
    );
    assert_eq!(hub.server_count(), 2);
}

/// A dropped client socket keeps reading until it closes; its frames must
/// not route or buffer for the connection it no longer belongs to.
#[tokio::test]
async fn frames_from_a_dropped_client_socket_are_ignored() {
    let hub = Arc::new(Hub::default());
    let key = (community(1), "srv".to_owned());
    let mut control = hub.register(&key, &Role::Control).unwrap();
    let client = hub.register(&key, &Role::Client("conn".into())).unwrap();
    hub.unregister(&key, &Role::Client("conn".into()), client.id);
    hub.forward(
        &key,
        &Role::Client("conn".into()),
        client.id,
        Message::Binary(vec![0u8; 1024].into()),
    );
    // The control socket's own notices count too; take them as its writer
    // would.
    take(&mut control);
    assert_eq!(hub.bytes_in_use(), 0, "a stale socket buffered a frame");
    assert_eq!(
        hub.register(&key, &Role::Data("conn".into())).err(),
        Some(Refusal::NoClient)
    );
}

/// Frames still queued when a socket ends are released, whatever ended it.
#[tokio::test]
async fn unsent_frames_are_released_when_a_socket_ends() {
    let hub = Arc::new(Hub::default());
    let key = (community(1), "srv".to_owned());
    let mut control = hub.register(&key, &Role::Control).unwrap();
    let client = hub.register(&key, &Role::Client("conn".into())).unwrap();
    let data = hub.register(&key, &Role::Data("conn".into())).unwrap();
    for _ in 0..3 {
        hub.forward(
            &key,
            &Role::Data("conn".into()),
            data.id,
            Message::Binary(vec![0u8; 1024].into()),
        );
    }
    take(&mut control);
    assert_eq!(hub.bytes_in_use(), 3 * 1024);
    drop(client);
    assert_eq!(hub.bytes_in_use(), 0);
}

/// A replaced data socket neither routes frames nor, when it closes, ends
/// the connection its successor now serves.
#[tokio::test]
async fn a_replaced_data_socket_has_no_effect() {
    let hub = Arc::new(Hub::default());
    let key = (community(1), "srv".to_owned());
    let connection = Role::Data("conn".into());
    let _control = hub.register(&key, &Role::Control).unwrap();
    let mut client = hub.register(&key, &Role::Client("conn".into())).unwrap();
    let mut old = hub.register(&key, &connection).unwrap();
    let new = hub.register(&key, &connection).unwrap();
    assert_eq!(closes(&take(&mut old)), [close::POLICY]);

    hub.forward(&key, &connection, old.id, Message::Text("stale".into()));
    hub.unregister(&key, &connection, old.id);
    assert!(take(&mut client).is_empty());

    hub.forward(&key, &connection, new.id, Message::Text("fresh".into()));
    assert!(matches!(
        take(&mut client).as_slice(),
        [Outbound::Frame(Message::Text(body))] if body.as_str() == "fresh"
    ));
}

/// A client that stops reading is dropped, and the daemon is told.
#[tokio::test]
async fn a_client_that_stops_reading_is_dropped() {
    let hub = Arc::new(Hub::default());
    let key = (community(1), "srv".to_owned());
    let mut control = hub.register(&key, &Role::Control).unwrap();
    let _stuck = hub.register(&key, &Role::Client("conn".into())).unwrap();
    let mut data = hub.register(&key, &Role::Data("conn".into())).unwrap();
    for _ in 0..=SEND_QUEUE {
        hub.forward(
            &key,
            &Role::Data("conn".into()),
            data.id,
            Message::Text("x".into()),
        );
    }
    assert!(notices(&take(&mut control))
        .contains(&json!({"type":"disconnected","connectionId":"conn"})));
    assert_eq!(closes(&take(&mut data)), [close::GOING_AWAY]);
}

/// A daemon data socket that stops reading is released, and its client is
/// told to reconnect.
#[tokio::test]
async fn a_daemon_that_stops_reading_is_released() {
    let hub = Arc::new(Hub::default());
    let key = (community(1), "srv".to_owned());
    let _control = hub.register(&key, &Role::Control).unwrap();
    let mut client = hub.register(&key, &Role::Client("conn".into())).unwrap();
    let _stuck = hub.register(&key, &Role::Data("conn".into())).unwrap();
    for _ in 0..=SEND_QUEUE {
        hub.forward(
            &key,
            &Role::Client("conn".into()),
            client.id,
            Message::Text("x".into()),
        );
    }
    assert_eq!(closes(&take(&mut client)), [close::RESTART]);
}

/// Buffered frames count against the process-wide byte budget; a connection
/// that would exceed it is closed and its buffer released.
#[tokio::test]
async fn the_byte_budget_closes_a_connection_that_exceeds_it() {
    let hub = Arc::new(Hub::with_limits(Limits {
        hub_bytes: 3 * MAX_MESSAGE_BYTES,
        ..Limits::default()
    }));
    let key = (community(1), "srv".to_owned());
    let mut control = hub.register(&key, &Role::Control).unwrap();
    let mut client = hub.register(&key, &Role::Client("conn".into())).unwrap();
    for _ in 0..4 {
        hub.forward(
            &key,
            &Role::Client("conn".into()),
            client.id,
            Message::Binary(vec![0u8; MAX_MESSAGE_BYTES].into()),
        );
    }
    assert_eq!(closes(&take(&mut client)), [close::TRY_AGAIN]);
    assert!(notices(&take(&mut control))
        .contains(&json!({"type":"disconnected","connectionId":"conn"})));
    assert_eq!(hub.bytes_in_use(), 0);
}

#[tokio::test]
async fn every_resource_is_bounded() {
    let hub = Arc::new(Hub::default());
    let key = (community(1), "srv".to_owned());
    let mut held = Vec::new();
    // Client sockets per connection.
    for _ in 0..MAX_CLIENT_SOCKETS {
        held.push(hub.register(&key, &Role::Client("conn".into())).unwrap());
    }
    assert_eq!(
        hub.register(&key, &Role::Client("conn".into())).err(),
        Some(Refusal::ClientSockets)
    );
    // Connections per server.
    for n in 1..MAX_CONNECTIONS_PER_SERVER {
        held.push(hub.register(&key, &Role::Client(format!("c{n}"))).unwrap());
    }
    assert_eq!(
        hub.register(&key, &Role::Client("one-more".into())).err(),
        Some(Refusal::Connections)
    );
    // Servers per community, then servers in the process.
    let hub = Arc::new(Hub::default());
    for n in 0..MAX_SERVERS_PER_COMMUNITY {
        held.push(
            hub.register(&(community(1), format!("s{n}")), &Role::Control)
                .unwrap(),
        );
    }
    assert_eq!(
        hub.register(&(community(1), "extra".into()), &Role::Control)
            .err(),
        Some(Refusal::Servers)
    );
    let communities = (MAX_SERVERS / MAX_SERVERS_PER_COMMUNITY) as u128;
    for n in MAX_SERVERS_PER_COMMUNITY..MAX_SERVERS {
        let community = community(2 + (n as u128 % (communities - 1)));
        held.push(
            hub.register(&(community, format!("s{n}")), &Role::Control)
                .unwrap(),
        );
    }
    assert_eq!(
        hub.register(&(community(99), "extra".into()), &Role::Control)
            .err(),
        Some(Refusal::Servers)
    );
    // Buffered frames keep the newest, within both per-connection bounds.
    let mut pending = Pending::new(Budget::default());
    for n in 0..MAX_PENDING_FRAMES + 50 {
        assert!(pending.push(Message::Text(n.to_string().into()), usize::MAX));
    }
    assert_eq!(pending.frames.len(), MAX_PENDING_FRAMES);
    assert_eq!(pending.frames.front(), Some(&Message::Text("50".into())));
    let mut pending = Pending::new(Budget::default());
    for _ in 0..8 {
        pending.push(
            Message::Binary(vec![0u8; MAX_MESSAGE_BYTES].into()),
            usize::MAX,
        );
    }
    assert_eq!(pending.bytes, MAX_PENDING_BYTES);
    assert_eq!(pending.frames.len(), MAX_PENDING_BYTES / MAX_MESSAGE_BYTES);
}
