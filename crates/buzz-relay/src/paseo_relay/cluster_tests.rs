//! Two replicas sharing the local Redis: each runs its own hub, cluster, and
//! `/ws` server, as two relay pods would. Skipped when Redis is unavailable.

use super::super::{parse, upgrade_to, Hub, Params, Target};
use super::*;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::Query;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use buzz_core::tenant::CommunityId;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as Ws;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const FAST: Timing = Timing {
    owner_ttl: Duration::from_secs(2),
    alive_ttl: Duration::from_millis(600),
    heartbeat: Duration::from_millis(150),
};

async fn redis_if_available() -> Option<(String, deadpool_redis::Pool)> {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let pool = deadpool_redis::Config::from_url(&url)
        .create_pool(Some(deadpool_redis::Runtime::Tokio1))
        .ok()?;
    let mut conn = pool.get().await.ok()?;
    redis::cmd("PING")
        .query_async::<String>(&mut *conn)
        .await
        .ok()?;
    Some((url, pool))
}

fn community() -> CommunityId {
    CommunityId::from_uuid(uuid::Uuid::from_u128(7))
}

struct Replica {
    cluster: Arc<Cluster>,
    hub: Arc<Hub>,
    base: String,
}

/// One relay replica: its own hub and cluster, serving `/ws` for one
/// community through the production routing and upgrade path.
async fn replica(url: &str, pool: &deadpool_redis::Pool) -> Replica {
    let hub = Arc::new(Hub::default());
    let cluster = Cluster::start_with(Arc::clone(&hub), pool.clone(), url.to_owned(), FAST);
    let (h, c) = (Arc::clone(&hub), Arc::clone(&cluster));
    let app = Router::new().route(
        "/ws",
        get(move |Query(params): Query<Params>, ws: WebSocketUpgrade| {
            let (hub, cluster) = (Arc::clone(&h), Arc::clone(&c));
            async move { routed(hub, cluster, params, ws).await }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !cluster.is_subscribed() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "inbox never subscribed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Replica {
        cluster,
        hub,
        base: format!("ws://{addr}/ws"),
    }
}

async fn routed(
    hub: Arc<Hub>,
    cluster: Arc<Cluster>,
    params: Params,
    ws: WebSocketUpgrade,
) -> Response {
    let (server, role) = match parse(params) {
        Ok(parsed) => parsed,
        Err(reason) => return (axum::http::StatusCode::BAD_REQUEST, reason).into_response(),
    };
    let key = (community(), server);
    let target = match cluster.route(&key).await {
        Ok(Route::Local) => Target::Local(hub),
        Ok(Route::Remote(owner)) => Target::Forwarded(cluster, owner),
        Err(error) => return (axum::http::StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    };
    upgrade_to(target, key, role, ws, ())
}

async fn open(base: &str, query: &str) -> Socket {
    connect_async(format!("{base}?{query}")).await.unwrap().0
}

async fn next_within(socket: &mut Socket, wait: Duration) -> Option<Ws> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let frame = tokio::time::timeout_at(deadline, socket.next())
            .await
            .ok()??
            .ok()?;
        if !matches!(frame, Ws::Ping(_) | Ws::Pong(_)) {
            return Some(frame);
        }
    }
}

async fn next(socket: &mut Socket) -> Ws {
    next_within(socket, Duration::from_secs(5))
        .await
        .expect("nothing within 5s")
}

async fn next_json(socket: &mut Socket) -> Value {
    match next(socket).await {
        Ws::Text(body) => serde_json::from_str(&body).unwrap(),
        other => panic!("expected JSON, got {other:?}"),
    }
}

async fn closed_with(socket: &mut Socket) -> u16 {
    match next(socket).await {
        Ws::Close(Some(frame)) => frame.code.into(),
        other => panic!("expected a close frame, got {other:?}"),
    }
}

async fn owner_of(pool: &deadpool_redis::Pool, server: &str) -> Option<String> {
    let mut conn = pool.get().await.unwrap();
    redis::cmd("GET")
        .arg(owner_key(&(community(), server.to_owned())))
        .query_async(&mut *conn)
        .await
        .unwrap()
}

fn server_id() -> String {
    format!("srv_{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

/// The daemon's control socket lands on replica A, which then owns the
/// server; the app and the daemon's data socket land on replica B and are
/// forwarded. The protocol runs exactly as on one replica.
#[tokio::test]
async fn a_session_spans_two_replicas() {
    let Some((url, pool)) = redis_if_available().await else {
        eprintln!("skipping two-replica Paseo relay test: Redis unavailable");
        return;
    };
    let (a, b) = (replica(&url, &pool).await, replica(&url, &pool).await);
    let server = server_id();

    let mut control = open(&a.base, &format!("serverId={server}&role=server&v=2")).await;
    assert_eq!(
        next_json(&mut control).await,
        json!({"type":"sync","connectionIds":[]})
    );
    assert_eq!(
        owner_of(&pool, &server).await.as_deref(),
        Some(a.cluster.replica())
    );

    let mut app = open(&b.base, &format!("serverId={server}&role=client&v=2")).await;
    let connected = next_json(&mut control).await;
    assert_eq!(connected["type"], "connected");
    let connection = connected["connectionId"].as_str().unwrap().to_owned();

    // Buffered on the owner, flushed to a data socket forwarded from B.
    app.send(Ws::text("hello")).await.unwrap();
    app.send(Ws::binary(vec![0u8, 255, 7])).await.unwrap();
    let data_query = format!("serverId={server}&role=server&v=2&connectionId={connection}");
    let mut data = open(&b.base, &data_query).await;
    assert_eq!(next(&mut data).await, Ws::text("hello"));
    assert_eq!(next(&mut data).await, Ws::binary(vec![0u8, 255, 7]));
    data.send(Ws::text("reply")).await.unwrap();
    assert_eq!(next(&mut app).await, Ws::text("reply"));
    assert_eq!(b.hub.server_count(), 0, "B holds no session of its own");

    // Releasing the connection crosses replicas too.
    app.close(None).await.unwrap();
    assert_eq!(closed_with(&mut data).await, close::GOING_AWAY);
    assert_eq!(
        next_json(&mut control).await,
        json!({"type":"disconnected","connectionId":connection})
    );

    // Once the session is empty, the owner releases the server, well before
    // the key would expire on its own.
    control.close(None).await.unwrap();
    let deadline = tokio::time::Instant::now() + FAST.owner_ttl / 2;
    while owner_of(&pool, &server).await.is_some() || a.hub.bytes_in_use() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "owner key or bytes not released"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// When the owning replica dies, the forwarding replica closes its sockets
/// (1012) once the owner stops refreshing its alive key, and the next socket
/// claims the server for a live replica without waiting out the owner key.
#[tokio::test]
async fn a_dead_owner_is_replaced() {
    let Some((url, pool)) = redis_if_available().await else {
        eprintln!("skipping Paseo relay owner failover test: Redis unavailable");
        return;
    };
    let (a, b) = (replica(&url, &pool).await, replica(&url, &pool).await);
    let server = server_id();

    let mut control = open(&a.base, &format!("serverId={server}&role=server&v=2")).await;
    assert_eq!(next_json(&mut control).await["type"], "sync");
    let mut app = open(&b.base, &format!("serverId={server}&role=client&v=2")).await;
    assert_eq!(next_json(&mut control).await["type"], "connected");

    a.cluster.halt();
    assert_eq!(closed_with(&mut app).await, close::RESTART);

    let _again = open(&b.base, &format!("serverId={server}&role=client&v=2")).await;
    assert_eq!(
        owner_of(&pool, &server).await.as_deref(),
        Some(b.cluster.replica())
    );
    assert_eq!(b.hub.server_count(), 1);
}

/// Ownership can move between a forwarding replica's lookup and its `open`
/// arriving. A replica that does not own the server refuses the socket
/// instead of starting a second session for it.
#[tokio::test]
async fn an_open_for_a_server_owned_elsewhere_starts_no_session() {
    let Some((url, pool)) = redis_if_available().await else {
        eprintln!("skipping Paseo relay ownership test: Redis unavailable");
        return;
    };
    let (a, b) = (replica(&url, &pool).await, replica(&url, &pool).await);
    a.cluster.handle(Envelope::Open {
        from: b.cluster.replica().to_owned(),
        sock: 1,
        community: *community().as_uuid(),
        server: server_id(),
        role: WireRole::Client("conn_x".into()),
    });
    assert_eq!(a.hub.server_count(), 0);
}

/// A replica that shuts down gracefully closes its sockets (1012), tells the
/// other replicas to close the sockets they forwarded to it, and releases
/// its server and liveness at once, so the next socket is claimed by a live
/// replica without waiting for any key to expire.
#[tokio::test]
async fn a_draining_owner_hands_its_servers_over_at_once() {
    let Some((url, pool)) = redis_if_available().await else {
        eprintln!("skipping Paseo relay drain test: Redis unavailable");
        return;
    };
    let (a, b) = (replica(&url, &pool).await, replica(&url, &pool).await);
    let server = server_id();
    let mut control = open(&a.base, &format!("serverId={server}&role=server&v=2")).await;
    assert_eq!(next_json(&mut control).await["type"], "sync");
    let mut app = open(&b.base, &format!("serverId={server}&role=client&v=2")).await;
    assert_eq!(next_json(&mut control).await["type"], "connected");

    a.cluster.shutdown().await;
    let quick = FAST.alive_ttl / 2;
    assert_eq!(
        next_within(&mut control, quick).await.map(close_code),
        Some(close::RESTART)
    );
    assert_eq!(
        next_within(&mut app, quick).await.map(close_code),
        Some(close::RESTART)
    );
    assert_eq!(owner_of(&pool, &server).await, None);
    let mut conn = pool.get().await.unwrap();
    let alive: bool = redis::cmd("EXISTS")
        .arg(alive_key(a.cluster.replica()))
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!(!alive);
    assert!(a.cluster.route(&(community(), server_id())).await.is_err());
}

fn close_code(frame: Ws) -> u16 {
    match frame {
        Ws::Close(Some(frame)) => frame.code.into(),
        other => panic!("expected a close frame, got {other:?}"),
    }
}
