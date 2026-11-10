//! Paseo relay protocol (v2) on `GET /ws`.
//!
//! [Paseo](https://paseo.sh) clients (desktop, mobile, CLI) reach a Paseo
//! daemon through a relay that both sides dial *out* to. The daemon and its
//! clients encrypt end to end (the pairing offer carries the daemon's public
//! key), so the relay only routes opaque frames. Serving the protocol here lets
//! an unmodified daemon and unmodified clients use this relay instead of
//! `relay.paseo.sh`: the daemon's `PASEO_RELAY_ENDPOINT` and
//! `PASEO_RELAY_PUBLIC_ENDPOINT` name this host, and the pairing offer tells
//! clients to use it.
//!
//! The behavior matches Paseo's reference relay (`@getpaseo/relay`
//! `cloudflare-adapter.js`, 0.15.3):
//!
//! | Socket | Query | Relay behavior |
//! |---|---|---|
//! | daemon control | `serverId=S&role=server&v=2` | one per server; replaced with 1008; receives `sync`, `connected`, `disconnected` |
//! | client | `serverId=S&role=client&v=2[&connectionId=C]` | `C` assigned when absent; frames buffer until the daemon's data socket arrives |
//! | daemon data | `serverId=S&role=server&v=2&connectionId=C` | one per connection; frames copy both ways with the client sockets of `C` |
//!
//! Sessions are keyed by the request's host-bound community and `serverId`,
//! so the community boundary holds. Protocol v1 (one unscoped pair per server)
//! is refused: current daemons use v2.
//!
//! Unlike the reference, which isolates each server in its own Durable Object,
//! every session a replica owns shares its relay process. So every resource is
//! bounded, including one byte budget for all buffered, queued, and
//! cross-replica frames, and sockets that stop answering pings are closed.
//!
//! With several relay replicas, [`Cluster`] routes each socket to the replica
//! that owns its server, over the relay's Redis (see `cluster.rs`).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use buzz_core::tenant::CommunityId;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;

/// Largest frame accepted. Paseo's hosted relay runs on Cloudflare, whose
/// WebSocket messages are capped at 1 MiB, so its clients already fit.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// Servers (daemons) with any socket open, across all communities.
pub const MAX_SERVERS: usize = 1024;
/// Servers with any socket open in one community, so one community cannot
/// exhaust [`MAX_SERVERS`] for the others.
pub const MAX_SERVERS_PER_COMMUNITY: usize = 256;
/// Distinct client connections per server. A daemon data socket exists only
/// for a connection with a client, so this also bounds data sockets.
pub const MAX_CONNECTIONS_PER_SERVER: usize = 64;
/// Client sockets sharing one connection id.
pub const MAX_CLIENT_SOCKETS: usize = 8;
/// Frames buffered for a connection whose daemon data socket is not open yet
/// (the reference relay keeps the newest 200).
pub const MAX_PENDING_FRAMES: usize = 200;
/// Bytes buffered for one such connection.
pub const MAX_PENDING_BYTES: usize = 4 * MAX_MESSAGE_BYTES;
/// Bytes queued for one socket's writer before the socket counts as stuck.
pub const MAX_QUEUED_BYTES: usize = 8 * MAX_MESSAGE_BYTES;
/// Bytes of buffered and queued frames across every Paseo socket in the
/// process. A frame that would exceed it closes the affected connection.
pub const MAX_HUB_BYTES: usize = 128 * MAX_MESSAGE_BYTES;
/// Frames queued for one socket's writer before the socket counts as stuck.
const SEND_QUEUE: usize = 256;
/// How long one write may take before the socket counts as stuck.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the relay pings each socket.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// A socket that sends nothing (not even a pong) for this long is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Longest control frame inspected for the legacy JSON ping.
const MAX_PING_BYTES: usize = 256;
/// After a client connects without a daemon data socket: resend `sync`, then
/// reset the daemon's control socket, then close the client so that no
/// connection waits (and buffers) indefinitely. Delays are cumulative.
const NUDGE_DELAYS: [Duration; 3] = [
    Duration::from_secs(10),
    Duration::from_secs(5),
    Duration::from_secs(45),
];
/// Longest `serverId` or `connectionId` accepted.
const MAX_ID_LEN: usize = 128;

/// WebSocket close codes the reference relay sends.
mod close {
    pub const GOING_AWAY: u16 = 1001;
    pub const POLICY: u16 = 1008;
    pub const INTERNAL: u16 = 1011;
    pub const RESTART: u16 = 1012;
    pub const TRY_AGAIN: u16 = 1013;
}

/// The `/ws` query string.
#[derive(Debug, Deserialize)]
pub struct Params {
    #[serde(rename = "serverId")]
    server_id: Option<String>,
    role: Option<String>,
    v: Option<String>,
    #[serde(rename = "connectionId")]
    connection_id: Option<String>,
}

/// One socket's place in a session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Role {
    Control,
    Data(String),
    Client(String),
}

type SessionKey = (CommunityId, String);

/// Validate the query. Errors are the reference relay's 400 bodies.
fn parse(params: Params) -> Result<(String, Role), &'static str> {
    let role = match params.role.as_deref() {
        Some(role @ ("server" | "client")) => role,
        _ => return Err("Missing or invalid role parameter"),
    };
    let server_id = params.server_id.unwrap_or_default();
    if server_id.is_empty() {
        return Err("Missing serverId parameter");
    }
    match params.v.as_deref().map(str::trim) {
        Some("2") => {}
        None | Some("") | Some("1") => {
            return Err("Relay protocol v1 is not supported (expected v=2)")
        }
        Some(_) => return Err("Invalid v parameter (expected 1 or 2)"),
    }
    let connection_id = params.connection_id.unwrap_or_default().trim().to_owned();
    if !valid_id(&server_id) || !(connection_id.is_empty() || valid_id(&connection_id)) {
        return Err("Invalid serverId or connectionId");
    }
    let role = match (role, connection_id.is_empty()) {
        ("server", true) => Role::Control,
        ("server", false) => Role::Data(connection_id),
        (_, true) => Role::Client(format!(
            "conn_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..16]
        )),
        (_, false) => Role::Client(connection_id),
    };
    Ok((server_id, role))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A shared byte counter with a ceiling.
#[derive(Clone, Default)]
struct Budget(Arc<AtomicUsize>);

impl Budget {
    fn reserve(&self, bytes: usize, max: usize) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= max)
            })
            .is_ok()
    }

    fn release(&self, bytes: usize) {
        self.0.fetch_sub(bytes, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn used(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

/// What a socket's writer task sends next.
#[derive(Debug)]
enum Outbound {
    Frame(Message),
    Close(u16, &'static str),
}

impl Outbound {
    fn bytes(&self) -> usize {
        match self {
            Self::Frame(frame) => frame_len(frame),
            Self::Close(..) => 0,
        }
    }
}

struct Sock {
    id: u64,
    tx: mpsc::Sender<Outbound>,
    /// Bytes waiting in this socket's queue.
    queued: Budget,
    /// The hub's byte budget, charged for the same bytes.
    hub: Budget,
    hub_max: usize,
}

impl Sock {
    /// Queue without waiting. False when the peer stopped reading (its queue
    /// is full) or the hub has no byte budget left.
    fn send(&self, outbound: Outbound) -> bool {
        let bytes = outbound.bytes();
        if !self.queued.reserve(bytes, MAX_QUEUED_BYTES) {
            return false;
        }
        if !self.hub.reserve(bytes, self.hub_max) {
            self.queued.release(bytes);
            return false;
        }
        if self.tx.try_send(outbound).is_err() {
            self.queued.release(bytes);
            self.hub.release(bytes);
            return false;
        }
        true
    }
}

#[derive(Default)]
struct Session {
    control: Option<Sock>,
    data: HashMap<String, Sock>,
    clients: HashMap<String, Vec<Sock>>,
    pending: HashMap<String, Pending>,
}

/// Frames a client sent before the daemon's data socket opened. Charged to
/// the hub budget until they are flushed or dropped.
struct Pending {
    frames: VecDeque<Message>,
    bytes: usize,
    hub: Budget,
}

impl Pending {
    fn new(hub: Budget) -> Self {
        Self {
            frames: VecDeque::new(),
            bytes: 0,
            hub,
        }
    }

    /// Buffer a frame, keeping the newest within the per-connection bounds.
    /// False when the hub budget cannot hold it.
    fn push(&mut self, frame: Message, hub_max: usize) -> bool {
        let bytes = frame_len(&frame);
        if !self.hub.reserve(bytes, hub_max) {
            return false;
        }
        self.bytes += bytes;
        self.frames.push_back(frame);
        while self.frames.len() > MAX_PENDING_FRAMES || self.bytes > MAX_PENDING_BYTES {
            if self.pop().is_none() {
                break;
            }
        }
        true
    }

    fn pop(&mut self) -> Option<Message> {
        let frame = self.frames.pop_front()?;
        let bytes = frame_len(&frame);
        self.bytes -= bytes;
        self.hub.release(bytes);
        Some(frame)
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.hub.release(self.bytes);
    }
}

fn frame_len(frame: &Message) -> usize {
    match frame {
        Message::Text(text) => text.len(),
        Message::Binary(bytes) => bytes.len(),
        _ => 0,
    }
}

fn text(value: serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}

impl Session {
    /// Pending frames exist only for a connection with clients (they go in
    /// [`Session::client_gone`]), so they never keep a session alive.
    fn is_empty(&self) -> bool {
        self.control.is_none() && self.data.is_empty() && self.clients.is_empty()
    }

    fn connection_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.clients.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn has_client(&self, connection: &str, id: u64) -> bool {
        self.clients
            .get(connection)
            .is_some_and(|sockets| sockets.iter().any(|sock| sock.id == id))
    }

    /// The daemon's data socket for `connection` is gone: make the client
    /// reconnect and handshake again.
    fn data_gone(&mut self, connection: &str) {
        self.data.remove(connection);
        if let Some(clients) = self.clients.get(connection) {
            for client in clients {
                let _ = client.send(Outbound::Close(close::RESTART, "Server disconnected"));
            }
        }
    }

    /// The last client socket for `connection` is gone: release the daemon's
    /// data socket and tell its control socket.
    fn client_gone(&mut self, connection: &str) {
        self.clients.remove(connection);
        self.pending.remove(connection);
        if let Some(data) = self.data.remove(connection) {
            let _ = data.send(Outbound::Close(close::GOING_AWAY, "Client disconnected"));
        }
        self.notify_control(serde_json::json!({
            "type": "disconnected", "connectionId": connection,
        }));
    }

    /// Close every client socket of `connection` and release it.
    fn end_connection(&mut self, connection: &str, code: u16, reason: &'static str) {
        if let Some(clients) = self.clients.get(connection) {
            for client in clients {
                let _ = client.send(Outbound::Close(code, reason));
            }
        }
        self.client_gone(connection);
    }

    fn notify_control(&mut self, message: serde_json::Value) {
        if let Some(control) = &self.control {
            if !control.send(Outbound::Frame(text(message))) {
                // A control socket that cannot take a notice is reset, so the
                // daemon reconnects and receives a fresh `sync`.
                if let Some(control) = self.control.take() {
                    let _ = control.send(Outbound::Close(close::INTERNAL, "Control send failed"));
                }
            }
        }
    }
}

/// Why a socket was refused after the upgrade.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// The process or community holds the most servers allowed.
    Servers,
    /// The server holds the most client connections allowed.
    Connections,
    /// The connection holds the most client sockets allowed.
    ClientSockets,
    /// A daemon data socket for a connection without a client.
    NoClient,
    /// The hub's byte budget cannot hold the connection's buffered frames.
    Capacity,
}

impl Refusal {
    fn close(&self) -> (u16, &'static str) {
        match self {
            Self::NoClient => (close::GOING_AWAY, "Client disconnected"),
            _ => (close::TRY_AGAIN, "Relay capacity reached"),
        }
    }
}

/// Bounds that tests shrink.
#[derive(Debug, Clone, Copy)]
struct Limits {
    hub_bytes: usize,
    nudge: [Duration; 3],
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            hub_bytes: MAX_HUB_BYTES,
            nudge: NUDGE_DELAYS,
        }
    }
}

/// A registered socket: its id, the receiver its writer drains, and the byte
/// counters the writer releases as frames leave. Dropping it releases every
/// frame still queued, so a socket that ends early returns its bytes.
struct Registered {
    id: u64,
    rx: mpsc::Receiver<Outbound>,
    queued: Budget,
    hub: Budget,
}

impl Registered {
    /// Release one frame's bytes after it leaves the queue.
    fn release(&self, outbound: &Outbound) {
        self.queued.release(outbound.bytes());
        self.hub.release(outbound.bytes());
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.rx.close();
        while let Ok(next) = self.rx.try_recv() {
            self.release(&next);
        }
    }
}

/// Routing state for every Paseo session on this relay process.
#[derive(Default)]
pub struct Hub {
    sessions: Mutex<HashMap<SessionKey, Session>>,
    next_id: AtomicU64,
    bytes: Budget,
    limits: Limits,
}

impl Hub {
    #[cfg(test)]
    fn with_limits(limits: Limits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<SessionKey, Session>> {
        // A panic while holding the lock leaves plain routing tables, which
        // stay consistent enough to keep serving.
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn new_sock(&self) -> (Sock, Registered) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(SEND_QUEUE);
        let queued = Budget::default();
        let sock = Sock {
            id,
            tx,
            queued: queued.clone(),
            hub: self.bytes.clone(),
            hub_max: self.limits.hub_bytes,
        };
        let hub = self.bytes.clone();
        (
            sock,
            Registered {
                id,
                rx,
                queued,
                hub,
            },
        )
    }

    /// Add a socket and run the protocol's side effects.
    fn register(self: &Arc<Self>, key: &SessionKey, role: &Role) -> Result<Registered, Refusal> {
        let (sock, registered) = self.new_sock();
        let mut sessions = self.sessions();
        if !sessions.contains_key(key) {
            let in_community = sessions.keys().filter(|(c, _)| *c == key.0).count();
            if sessions.len() >= MAX_SERVERS || in_community >= MAX_SERVERS_PER_COMMUNITY {
                return Err(Refusal::Servers);
            }
        }
        let session = sessions.entry(key.clone()).or_default();
        match role {
            Role::Control => {
                let sync = json_sync(session);
                if let Some(old) = session.control.replace(sock) {
                    let _ = old.send(Outbound::Close(close::POLICY, "Replaced by new connection"));
                }
                session.notify_control(sync);
            }
            Role::Data(connection) => {
                if !session.clients.contains_key(connection) {
                    drop_if_empty(&mut sessions, key);
                    return Err(Refusal::NoClient);
                }
                // Flush what the client sent before this socket existed.
                if let Some(mut pending) = session.pending.remove(connection) {
                    while let Some(frame) = pending.pop() {
                        if !sock.send(Outbound::Frame(frame)) {
                            session.end_connection(
                                connection,
                                close::TRY_AGAIN,
                                "Relay capacity reached",
                            );
                            drop_if_empty(&mut sessions, key);
                            // Dropping the registration releases what the
                            // flush already queued.
                            return Err(Refusal::Capacity);
                        }
                    }
                }
                if let Some(old) = session.data.insert(connection.clone(), sock) {
                    let _ = old.send(Outbound::Close(close::POLICY, "Replaced by new connection"));
                }
            }
            Role::Client(connection) => {
                let known = session.clients.contains_key(connection);
                if !known && session.clients.len() >= MAX_CONNECTIONS_PER_SERVER {
                    drop_if_empty(&mut sessions, key);
                    return Err(Refusal::Connections);
                }
                let sockets = session.clients.entry(connection.clone()).or_default();
                if sockets.len() >= MAX_CLIENT_SOCKETS {
                    return Err(Refusal::ClientSockets);
                }
                sockets.push(sock);
                session.notify_control(serde_json::json!({
                    "type": "connected", "connectionId": connection,
                }));
                drop(sessions);
                self.nudge(key.clone(), connection.clone());
            }
        }
        Ok(registered)
    }

    /// Route one frame from a socket. Only the socket the session currently
    /// holds for its role routes: a replaced or dropped socket's frames are
    /// ignored until it closes.
    fn forward(&self, key: &SessionKey, role: &Role, id: u64, frame: Message) {
        // The only control frame the relay acts on is the legacy JSON ping,
        // which is tiny: inspect it before taking the lock.
        if *role == Role::Control && !is_legacy_ping(&frame) {
            return;
        }
        let mut sessions = self.sessions();
        let Some(session) = sessions.get_mut(key) else {
            return;
        };
        match role {
            Role::Control => {
                // COMPAT: daemons before Paseo 0.1.76 keep the control socket
                // alive with JSON pings instead of WebSocket pings.
                if session.control.as_ref().is_some_and(|c| c.id == id) {
                    session.notify_control(serde_json::json!({
                        "type": "pong",
                        "ts": chrono::Utc::now().timestamp_millis(),
                    }));
                }
            }
            Role::Client(connection) => {
                if !session.has_client(connection, id) {
                    return;
                }
                match session.data.get(connection) {
                    Some(data) => {
                        if !data.send(Outbound::Frame(frame)) {
                            session.data_gone(connection);
                        }
                    }
                    None => {
                        let hub = self.bytes.clone();
                        let buffered = session
                            .pending
                            .entry(connection.clone())
                            .or_insert_with(|| Pending::new(hub))
                            .push(frame, self.limits.hub_bytes);
                        if !buffered {
                            session.end_connection(
                                connection,
                                close::TRY_AGAIN,
                                "Relay capacity reached",
                            );
                        }
                    }
                }
            }
            Role::Data(connection) => {
                if session.data.get(connection).is_some_and(|d| d.id == id) {
                    if let Some(clients) = session.clients.get_mut(connection) {
                        // A client that stopped reading is dropped.
                        clients.retain(|client| client.send(Outbound::Frame(frame.clone())));
                        if clients.is_empty() {
                            session.client_gone(connection);
                        }
                    }
                }
            }
        }
        drop_if_empty(&mut sessions, key);
    }

    /// Remove a closed socket and run the protocol's side effects. A socket
    /// that was already replaced has no effects: its successor is serving.
    fn unregister(&self, key: &SessionKey, role: &Role, id: u64) {
        let mut sessions = self.sessions();
        let Some(session) = sessions.get_mut(key) else {
            return;
        };
        match role {
            Role::Control => {
                if session.control.as_ref().is_some_and(|c| c.id == id) {
                    session.control = None;
                }
            }
            Role::Client(connection) => {
                if let Some(sockets) = session.clients.get_mut(connection) {
                    sockets.retain(|sock| sock.id != id);
                    if sockets.is_empty() {
                        session.client_gone(connection);
                    }
                }
            }
            Role::Data(connection) => {
                if session.data.get(connection).is_some_and(|d| d.id == id) {
                    session.data_gone(connection);
                }
            }
        }
        drop_if_empty(&mut sessions, key);
    }

    /// A half-open control socket accepts writes without reading them, so
    /// watch whether the daemon opens the data socket for a new client. If it
    /// does not, resend `sync`; if it still does not, reset the control
    /// socket; if the daemon never answers, close the client.
    fn nudge(self: &Arc<Self>, key: SessionKey, connection: String) {
        let hub = Arc::downgrade(self);
        let delays = self.limits.nudge;
        tokio::spawn(async move {
            for (step, delay) in delays.into_iter().enumerate() {
                tokio::time::sleep(delay).await;
                let Some(hub) = hub.upgrade() else { return };
                let mut sessions = hub.sessions();
                let Some(session) = sessions.get_mut(&key) else {
                    return;
                };
                if !session.clients.contains_key(&connection)
                    || session.data.contains_key(&connection)
                {
                    return;
                }
                match step {
                    0 => {
                        let sync = json_sync(session);
                        session.notify_control(sync);
                    }
                    1 => {
                        if let Some(control) = session.control.take() {
                            let _ = control
                                .send(Outbound::Close(close::INTERNAL, "Control unresponsive"));
                        }
                    }
                    _ => {
                        session.end_connection(&connection, close::TRY_AGAIN, "Server unavailable");
                        drop_if_empty(&mut sessions, &key);
                    }
                }
            }
        });
    }

    fn session_keys(&self) -> Vec<SessionKey> {
        self.sessions().keys().cloned().collect()
    }

    fn has_session(&self, key: &SessionKey) -> bool {
        self.sessions().contains_key(key)
    }

    /// End every socket of `key`'s session and forget it: another replica
    /// now owns the server, so its sockets must reconnect there.
    fn evict(&self, key: &SessionKey) {
        let Some(session) = self.sessions().remove(key) else {
            return;
        };
        let sockets = session
            .control
            .iter()
            .chain(session.data.values())
            .chain(session.clients.values().flatten());
        for sock in sockets {
            let _ = sock.send(Outbound::Close(close::RESTART, "Server disconnected"));
        }
    }

    #[cfg(test)]
    fn server_count(&self) -> usize {
        self.sessions().len()
    }

    #[cfg(test)]
    fn bytes_in_use(&self) -> usize {
        self.bytes.used()
    }
}

fn is_legacy_ping(frame: &Message) -> bool {
    matches!(frame, Message::Text(body) if body.len() <= MAX_PING_BYTES
        && serde_json::from_str::<serde_json::Value>(body.as_str())
            .is_ok_and(|value| value["type"] == "ping"))
}

fn json_sync(session: &Session) -> serde_json::Value {
    serde_json::json!({"type": "sync", "connectionIds": session.connection_ids()})
}

fn drop_if_empty(sessions: &mut HashMap<SessionKey, Session>, key: &SessionKey) {
    if sessions.get(key).is_some_and(Session::is_empty) {
        sessions.remove(key);
    }
}

/// State for the `/ws` route.
#[derive(Clone)]
pub struct RelayState {
    /// The relay, for host binding, shutdown, and the connection limit.
    pub app: Arc<crate::state::AppState>,
    /// Paseo routing tables for servers this replica owns.
    pub hub: Arc<Hub>,
    /// Routing to servers other replicas own.
    pub cluster: Arc<Cluster>,
}

/// `GET /ws`: validate the query, bind the community from the request host,
/// find the replica that owns the server, then upgrade.
///
/// A malformed query is refused (400) before any host lookup, and a draining
/// relay accepts no socket (503). Like the main relay door, an unmapped host
/// fails closed with a generic 404 before the upgrade. If the owner cannot be
/// found (Redis unavailable), the request gets 503. Each socket holds one
/// relay connection permit.
pub async fn ws_handler(
    State(state): State<RelayState>,
    headers: HeaderMap,
    Query(params): Query<Params>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let parsed = match parse(params) {
        Ok(parsed) => parsed,
        Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
    };
    if state.app.shutting_down.load(Ordering::Acquire) {
        return (StatusCode::SERVICE_UNAVAILABLE, "relay: shutting down").into_response();
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    let raw_host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Ok(tenant) = crate::tenant::bind_community(&*state.app, raw_host).await else {
        return (
            StatusCode::NOT_FOUND,
            "relay: no community is configured for this host",
        )
            .into_response();
    };
    let Ok(permit) = Arc::clone(&state.app.conn_semaphore).try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "relay: connection limit reached",
        )
            .into_response();
    };
    let (server_id, role) = parsed;
    let key = (tenant.community(), server_id);
    let target = match state.cluster.route(&key).await {
        Ok(cluster::Route::Local) => Target::Local(state.hub),
        Ok(cluster::Route::Remote(owner)) => Target::Forwarded(state.cluster, owner),
        Err(error) => {
            tracing::warn!(%error, "paseo relay could not route a socket");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "relay: routing unavailable",
            )
                .into_response();
        }
    };
    upgrade_to(target, key, role, ws, permit)
}

/// Which replica serves a socket's session.
enum Target {
    /// This replica owns the server.
    Local(Arc<Hub>),
    /// Another replica owns the server; frames go to it through the cluster.
    Forwarded(Arc<Cluster>, String),
}

/// The upgrade path below the host binding, for tests that serve one fixed
/// community.
#[cfg(test)]
fn upgrade(
    hub: Arc<Hub>,
    community: CommunityId,
    params: Params,
    ws: WebSocketUpgrade,
) -> Response {
    match parse(params) {
        Ok((server_id, role)) => {
            upgrade_to(Target::Local(hub), (community, server_id), role, ws, ())
        }
        Err(reason) => (StatusCode::BAD_REQUEST, reason).into_response(),
    }
}

fn upgrade_to<P: Send + 'static>(
    target: Target,
    key: SessionKey,
    role: Role,
    ws: WebSocketUpgrade,
    permit: P,
) -> Response {
    ws.max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            match target {
                Target::Local(hub) => serve_local(hub, key, role, socket).await,
                Target::Forwarded(cluster, owner) => {
                    serve_forwarded(cluster, key, role, owner, socket).await
                }
            }
            drop(permit);
        })
}

/// Serve a socket whose session this replica's hub holds.
async fn serve_local(hub: Arc<Hub>, key: SessionKey, role: Role, mut socket: WebSocket) {
    let registered = match hub.register(&key, &role) {
        Ok(registered) => registered,
        Err(refusal) => {
            tracing::warn!(?refusal, "paseo relay refused a socket");
            let (code, reason) = refusal.close();
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code,
                    reason: reason.into(),
                })))
                .await;
            return;
        }
    };
    let id = registered.id;
    pump(socket, registered, |frame| {
        hub.forward(&key, &role, id, frame)
    })
    .await;
    hub.unregister(&key, &role, id);
}

/// Serve a socket whose session another replica owns: its frames go to the
/// owner, and the owner's frames for it arrive on its queue.
async fn serve_forwarded(
    cluster: Arc<Cluster>,
    key: SessionKey,
    role: Role,
    owner: String,
    socket: WebSocket,
) {
    let registered = cluster.open_forwarded(&key, &role, &owner);
    let id = registered.id;
    pump(socket, registered, |frame| {
        cluster.frame_to_owner(id, &frame)
    })
    .await;
    cluster.closed_forwarded(id);
}

/// Run one socket: send what its queue holds (and keepalive pings), hand each
/// data frame it receives to `forward`, and return when either side ends.
/// Dropping `registered` afterwards releases whatever was never sent.
async fn pump(socket: WebSocket, mut registered: Registered, forward: impl Fn(Message)) {
    let started = tokio::time::Instant::now();
    let last_seen = AtomicU64::new(0);
    let (mut sink, mut stream) = socket.split();
    let writer = async {
        let mut ping = tokio::time::interval_at(started + PING_INTERVAL, PING_INTERVAL);
        loop {
            let (frame, last) = tokio::select! {
                next = registered.rx.recv() => match next {
                    Some(next) => {
                        registered.release(&next);
                        match next {
                            Outbound::Frame(frame) => (frame, false),
                            Outbound::Close(code, reason) => (
                                Message::Close(Some(CloseFrame { code, reason: reason.into() })),
                                true,
                            ),
                        }
                    }
                    None => break,
                },
                _ = ping.tick() => {
                    let quiet = (started.elapsed().as_millis() as u64)
                        .saturating_sub(last_seen.load(Ordering::Acquire));
                    if quiet > IDLE_TIMEOUT.as_millis() as u64 {
                        break;
                    }
                    (Message::Ping(Default::default()), false)
                }
            };
            if !matches!(
                tokio::time::timeout(SEND_TIMEOUT, sink.send(frame)).await,
                Ok(Ok(()))
            ) || last
            {
                break;
            }
        }
    };
    let reader = async {
        while let Some(Ok(frame)) = stream.next().await {
            last_seen.store(started.elapsed().as_millis() as u64, Ordering::Release);
            match frame {
                Message::Text(_) | Message::Binary(_) => forward(frame),
                Message::Close(_) => break,
                // tungstenite answers pings itself; a pong only proves life.
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    };
    // Either side ending ends the socket. When the hub drops this socket's
    // sender (replaced, or its queue filled), the writer ends after draining.
    tokio::select! {
        () = writer => {}
        () = reader => {}
    }
}

mod cluster;
pub use cluster::Cluster;

#[cfg(test)]
mod tests;
