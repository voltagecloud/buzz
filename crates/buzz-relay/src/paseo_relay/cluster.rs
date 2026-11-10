//! Cross-replica routing for the Paseo relay, over the relay's Redis.
//!
//! One relay replica owns each Paseo server: it holds the server's session in
//! its [`Hub`], which applies every protocol rule. A replica that accepts a
//! socket for a server another replica owns forwards that socket's frames to
//! the owner over Redis pub/sub, and the owner sends the socket's outbound
//! frames back the same way. With one replica, every server is local, and the
//! only Redis traffic is the ownership claim and its heartbeat.
//!
//! ```text
//! app ─ws─▶ replica B ──open/frame/closed──▶ inbox:A ─▶ replica A (owner, Hub) ◀─ws─ daemon
//!                      ◀──deliver/close───── inbox:B ◀─
//! ```
//!
//! - **Ownership:** `owner:{community}:{serverId}` holds the owning replica,
//!   claimed with `SET NX PX`, refreshed by heartbeat, and released with a
//!   compare-and-delete once the session is empty. Each replica refreshes
//!   `alive:{replica}`; a claim takes over an owner key whose replica is no
//!   longer alive.
//! - **Inbox:** each replica subscribes to one channel, `replica:{id}`.
//!   One publisher task on one connection keeps a replica's messages to
//!   another replica in order.
//! - **Loss:** a subscriber reconnect may have lost messages, so every
//!   cross-replica socket is closed and Paseo reconnects. A publish that fails
//!   or reaches no subscriber closes the affected socket. An owner whose key
//!   was taken evicts the session; a forwarding replica whose owner died closes
//!   its forwarded sockets.
//!
//! Keys are built inside one Lua script (the claim), so this assumes a
//! standalone Redis, which is what the relay deploys.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use axum::extract::ws::Message;
use base64::Engine as _;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{close, Hub, Outbound, Registered, Role, SessionKey, Sock};

const PREFIX: &str = "buzz:paseo:v1";
/// Envelopes queued for the publisher. Their bytes also count against the
/// hub's byte budget.
const OUTBOX: usize = 4096;
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
/// How long graceful shutdown waits for queued closes to be published.
const SHUTDOWN_FLUSH: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Claim `KEYS[1]` (owner) for `ARGV[1]` (this replica) unless a live replica
/// holds it, refresh `KEYS[2]` (this replica's alive key), and return the
/// owner. `ARGV[3]` is the alive-key prefix of other replicas.
const CLAIM: &str = r"
redis.call('SET', KEYS[2], '1', 'PX', ARGV[4])
local current = redis.call('GET', KEYS[1])
if current == ARGV[1] then
  redis.call('PEXPIRE', KEYS[1], ARGV[2])
  return current
end
if current and redis.call('EXISTS', ARGV[3] .. current) == 1 then
  return current
end
redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])
return ARGV[1]
";
/// Refresh `KEYS[1]` only while `ARGV[1]` holds it. Returns 0 when lost.
const REFRESH: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('PEXPIRE', KEYS[1], ARGV[2])
end
return 0
";
/// Delete `KEYS[1]` only while `ARGV[1]` holds it.
const RELEASE: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
end
return 0
";

fn owner_key((community, server): &SessionKey) -> String {
    format!("{PREFIX}:owner:{community}:{server}")
}

fn alive_prefix() -> String {
    format!("{PREFIX}:alive:")
}

fn alive_key(replica: &str) -> String {
    format!("{}{replica}", alive_prefix())
}

fn inbox(replica: &str) -> String {
    format!("{PREFIX}:replica:{replica}")
}

/// Timing that tests shrink.
#[derive(Debug, Clone, Copy)]
pub(super) struct Timing {
    pub(super) owner_ttl: Duration,
    pub(super) alive_ttl: Duration,
    pub(super) heartbeat: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            owner_ttl: Duration::from_secs(30),
            alive_ttl: Duration::from_secs(15),
            heartbeat: Duration::from_secs(5),
        }
    }
}

/// Where a new socket's session lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Route {
    /// This replica owns the server.
    Local,
    /// Another live replica owns the server.
    Remote(String),
}

/// Messages between replicas.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Envelope {
    /// Forwarding replica to owner: a socket opened.
    Open {
        from: String,
        sock: u64,
        community: uuid::Uuid,
        server: String,
        role: WireRole,
    },
    /// Forwarding replica to owner: the socket sent a frame.
    Frame {
        from: String,
        sock: u64,
        payload: Payload,
    },
    /// Forwarding replica to owner: the socket closed.
    Closed { from: String, sock: u64 },
    /// Owner to forwarding replica: send this frame on the socket.
    Deliver { sock: u64, payload: Payload },
    /// Owner to forwarding replica: close the socket.
    Close { sock: u64, code: u16 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "k", content = "v", rename_all = "snake_case")]
enum WireRole {
    Control,
    Data(String),
    Client(String),
}

impl From<&Role> for WireRole {
    fn from(role: &Role) -> Self {
        match role {
            Role::Control => Self::Control,
            Role::Data(c) => Self::Data(c.clone()),
            Role::Client(c) => Self::Client(c.clone()),
        }
    }
}

impl From<WireRole> for Role {
    fn from(role: WireRole) -> Self {
        match role {
            WireRole::Control => Self::Control,
            WireRole::Data(c) => Self::Data(c),
            WireRole::Client(c) => Self::Client(c),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "k", content = "v", rename_all = "snake_case")]
enum Payload {
    Text(String),
    /// Base64 (standard alphabet).
    Binary(String),
}

impl Payload {
    fn from_message(frame: &Message) -> Option<Self> {
        match frame {
            Message::Text(text) => Some(Self::Text(text.as_str().to_owned())),
            Message::Binary(bytes) => Some(Self::Binary(
                base64::engine::general_purpose::STANDARD.encode(bytes),
            )),
            _ => None,
        }
    }

    fn into_message(self) -> Option<Message> {
        match self {
            Self::Text(text) => Some(Message::Text(text.into())),
            Self::Binary(encoded) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
                .map(|bytes| Message::Binary(bytes.into())),
        }
    }
}

/// The reason sent with a close code that crossed replicas.
fn reason_for(code: u16) -> &'static str {
    match code {
        close::GOING_AWAY => "Client disconnected",
        close::POLICY => "Replaced by new connection",
        close::INTERNAL => "Control unresponsive",
        close::RESTART => "Server disconnected",
        _ => "Relay capacity reached",
    }
}

/// What to undo when an envelope cannot be published.
#[derive(Debug)]
enum OnFailure {
    /// Close this forwarded local socket.
    CloseForwarded(u64),
    /// Drop this remote socket from the owner's session.
    DropRemote(String, u64),
    Nothing,
}

struct Outgoing {
    channel: String,
    payload: String,
    on_failure: OnFailure,
}

/// Owner side: a socket another replica forwards.
struct RemoteSock {
    hub_id: u64,
    key: SessionKey,
    role: Role,
}

/// Forwarding side: a local socket whose server another replica owns.
struct Forwarded {
    key: SessionKey,
    owner: String,
    sock: Sock,
}

/// Cross-replica routing for one relay process.
pub struct Cluster {
    replica: String,
    hub: Arc<Hub>,
    pool: deadpool_redis::Pool,
    timing: Timing,
    outbox: mpsc::Sender<Outgoing>,
    subscribed: AtomicBool,
    /// Set once graceful shutdown starts: no new servers are claimed.
    draining: AtomicBool,
    stop: CancellationToken,
    /// Servers this replica owns.
    owned: Mutex<HashSet<SessionKey>>,
    /// Owner side: sockets forwarded by other replicas, by (replica, socket).
    remote: Mutex<HashMap<(String, u64), RemoteSock>>,
    /// Forwarding side: local sockets by id.
    forwarded: Mutex<HashMap<u64, Forwarded>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Cluster {
    /// Start routing for `hub`: the subscriber, publisher, and heartbeat
    /// tasks. They stop when the returned cluster is dropped.
    pub fn start(hub: Arc<Hub>, pool: deadpool_redis::Pool, redis_url: String) -> Arc<Self> {
        Self::start_with(hub, pool, redis_url, Timing::default())
    }

    pub(super) fn start_with(
        hub: Arc<Hub>,
        pool: deadpool_redis::Pool,
        redis_url: String,
        timing: Timing,
    ) -> Arc<Self> {
        let (outbox, outgoing) = mpsc::channel(OUTBOX);
        let cluster = Arc::new(Self {
            replica: uuid::Uuid::new_v4().simple().to_string(),
            hub,
            pool,
            timing,
            outbox,
            subscribed: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            stop: CancellationToken::new(),
            owned: Mutex::default(),
            remote: Mutex::default(),
            forwarded: Mutex::default(),
        });
        let weak = Arc::downgrade(&cluster);
        spawn_until(
            &cluster.stop,
            publish_loop(weak.clone(), redis_url.clone(), outgoing),
        );
        spawn_until(
            &cluster.stop,
            subscribe_loop(weak.clone(), redis_url, cluster.replica.clone()),
        );
        spawn_until(&cluster.stop, heartbeat_loop(weak, timing.heartbeat));
        cluster
    }

    /// Hand this replica's servers to the others when `shutdown` is
    /// cancelled (graceful shutdown).
    pub fn drain_on(self: &Arc<Self>, shutdown: CancellationToken) {
        let cluster = Arc::downgrade(self);
        tokio::spawn(async move {
            shutdown.cancelled().await;
            if let Some(cluster) = cluster.upgrade() {
                cluster.shutdown().await;
            }
        });
    }

    /// Graceful handoff: close every socket this replica serves (1012) so
    /// that daemons and apps reconnect to another replica, let the closes
    /// for forwarded sockets reach their replicas, then release this
    /// replica's servers and liveness so the next socket claims them at once
    /// instead of waiting for the keys to expire. Stops this replica's tasks.
    pub(super) async fn shutdown(&self) {
        self.draining.store(true, Ordering::Release);
        for key in self.hub.session_keys() {
            self.hub.evict(&key);
        }
        let forwarded: Vec<u64> = locked(&self.forwarded).keys().copied().collect();
        for id in forwarded {
            self.close_forwarded(id, close::RESTART);
        }
        // Owner-side closes travel through each socket's relay task and then
        // the publisher; give them a bounded moment to leave.
        let deadline = tokio::time::Instant::now() + SHUTDOWN_FLUSH;
        while self.outbox.capacity() < OUTBOX && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if let Ok(mut conn) = self.pool.get().await {
            let owned: Vec<SessionKey> = locked(&self.owned).drain().collect();
            for key in owned {
                let _: Result<i64, _> = redis::Script::new(RELEASE)
                    .key(owner_key(&key))
                    .arg(&self.replica)
                    .invoke_async(&mut *conn)
                    .await;
            }
            let _: Result<i64, _> = redis::cmd("DEL")
                .arg(alive_key(&self.replica))
                .query_async(&mut *conn)
                .await;
        }
        self.stop.cancel();
    }

    /// Claim or find the owner of `key`'s server.
    pub(super) async fn route(&self, key: &SessionKey) -> Result<Route, String> {
        if self.draining.load(Ordering::Acquire) {
            return Err("paseo relay replica is shutting down".into());
        }
        let mut conn = self.pool.get().await.map_err(|e| e.to_string())?;
        let owner: String = redis::Script::new(CLAIM)
            .key(owner_key(key))
            .key(alive_key(&self.replica))
            .arg(&self.replica)
            .arg(self.timing.owner_ttl.as_millis() as u64)
            .arg(alive_prefix())
            .arg(self.timing.alive_ttl.as_millis() as u64)
            .invoke_async(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
        if owner == self.replica {
            locked(&self.owned).insert(key.clone());
            return Ok(Route::Local);
        }
        if !self.subscribed.load(Ordering::Acquire) {
            return Err("paseo relay inbox is not subscribed".into());
        }
        Ok(Route::Remote(owner))
    }

    /// Forwarding side: register a local socket for a server `owner` owns.
    pub(super) fn open_forwarded(&self, key: &SessionKey, role: &Role, owner: &str) -> Registered {
        let (sock, registered) = self.hub.new_sock();
        let id = registered.id;
        locked(&self.forwarded).insert(
            id,
            Forwarded {
                key: key.clone(),
                owner: owner.to_owned(),
                sock,
            },
        );
        self.enqueue(
            owner,
            &Envelope::Open {
                from: self.replica.clone(),
                sock: id,
                community: *key.0.as_uuid(),
                server: key.1.clone(),
                role: role.into(),
            },
            OnFailure::CloseForwarded(id),
        );
        registered
    }

    /// Forwarding side: send a frame from local socket `id` to its owner.
    pub(super) fn frame_to_owner(&self, id: u64, frame: &Message) {
        let Some(payload) = Payload::from_message(frame) else {
            return;
        };
        let Some(owner) = locked(&self.forwarded).get(&id).map(|f| f.owner.clone()) else {
            return;
        };
        self.enqueue(
            &owner,
            &Envelope::Frame {
                from: self.replica.clone(),
                sock: id,
                payload,
            },
            OnFailure::CloseForwarded(id),
        );
    }

    /// Forwarding side: local socket `id` ended.
    pub(super) fn closed_forwarded(&self, id: u64) {
        if let Some(forwarded) = locked(&self.forwarded).remove(&id) {
            self.enqueue(
                &forwarded.owner,
                &Envelope::Closed {
                    from: self.replica.clone(),
                    sock: id,
                },
                OnFailure::Nothing,
            );
        }
    }

    /// Forwarding side: close local socket `id` and tell its owner.
    fn close_forwarded(&self, id: u64, code: u16) {
        if let Some(forwarded) = locked(&self.forwarded).remove(&id) {
            let _ = forwarded.sock.send(Outbound::Close(code, reason_for(code)));
            self.enqueue(
                &forwarded.owner,
                &Envelope::Closed {
                    from: self.replica.clone(),
                    sock: id,
                },
                OnFailure::Nothing,
            );
        }
    }

    /// Owner side: drop a remote socket from its session.
    fn drop_remote(&self, from: &str, sock: u64) {
        if let Some(remote) = locked(&self.remote).remove(&(from.to_owned(), sock)) {
            self.hub
                .unregister(&remote.key, &remote.role, remote.hub_id);
        }
    }

    fn fail(&self, on_failure: OnFailure) {
        match on_failure {
            OnFailure::CloseForwarded(id) => self.close_forwarded(id, close::RESTART),
            OnFailure::DropRemote(from, sock) => self.drop_remote(&from, sock),
            OnFailure::Nothing => {}
        }
    }

    /// Queue an envelope for `replica`'s inbox, charging its bytes to the
    /// hub's budget until it is published.
    fn enqueue(&self, replica: &str, envelope: &Envelope, on_failure: OnFailure) {
        let Ok(payload) = serde_json::to_string(envelope) else {
            return self.fail(on_failure);
        };
        if !self
            .hub
            .bytes
            .reserve(payload.len(), self.hub.limits.hub_bytes)
        {
            return self.fail(on_failure);
        }
        let bytes = payload.len();
        let outgoing = Outgoing {
            channel: inbox(replica),
            payload,
            on_failure,
        };
        if let Err(error) = self.outbox.try_send(outgoing) {
            self.hub.bytes.release(bytes);
            let outgoing = match error {
                mpsc::error::TrySendError::Full(o) | mpsc::error::TrySendError::Closed(o) => o,
            };
            self.fail(outgoing.on_failure);
        }
    }

    /// Handle one envelope from this replica's inbox.
    fn handle(self: &Arc<Self>, envelope: Envelope) {
        match envelope {
            Envelope::Open {
                from,
                sock,
                community,
                server,
                role,
            } => {
                let key = (buzz_core::tenant::CommunityId::from_uuid(community), server);
                let role = Role::from(role);
                if !locked(&self.owned).contains(&key) {
                    // Ownership moved since the sender looked it up.
                    self.enqueue(
                        &from,
                        &Envelope::Close {
                            sock,
                            code: close::RESTART,
                        },
                        OnFailure::Nothing,
                    );
                    return;
                }
                match self.hub.register(&key, &role) {
                    Ok(registered) => {
                        locked(&self.remote).insert(
                            (from.clone(), sock),
                            RemoteSock {
                                hub_id: registered.id,
                                key,
                                role,
                            },
                        );
                        tokio::spawn(relay_out(Arc::downgrade(self), registered, from, sock));
                    }
                    Err(refusal) => {
                        let (code, _) = refusal.close();
                        self.enqueue(&from, &Envelope::Close { sock, code }, OnFailure::Nothing);
                    }
                }
            }
            Envelope::Frame {
                from,
                sock,
                payload,
            } => {
                let target = locked(&self.remote)
                    .get(&(from, sock))
                    .map(|r| (r.key.clone(), r.role.clone(), r.hub_id));
                if let (Some((key, role, hub_id)), Some(frame)) = (target, payload.into_message()) {
                    self.hub.forward(&key, &role, hub_id, frame);
                }
            }
            Envelope::Closed { from, sock } => self.drop_remote(&from, sock),
            Envelope::Deliver { sock, payload } => {
                let Some(frame) = payload.into_message() else {
                    return;
                };
                let delivered = locked(&self.forwarded)
                    .get(&sock)
                    .is_some_and(|f| f.sock.send(Outbound::Frame(frame)));
                if !delivered {
                    // The socket stopped reading (or is gone).
                    self.close_forwarded(sock, close::TRY_AGAIN);
                }
            }
            Envelope::Close { sock, code } => {
                if let Some(forwarded) = locked(&self.forwarded).remove(&sock) {
                    let _ = forwarded.sock.send(Outbound::Close(code, reason_for(code)));
                }
            }
        }
    }

    /// Messages may have been lost: end every cross-replica socket so that
    /// both sides reconnect from a clean state.
    fn reset_cross_replica(&self) {
        let forwarded: Vec<u64> = locked(&self.forwarded).keys().copied().collect();
        for id in forwarded {
            self.close_forwarded(id, close::RESTART);
        }
        let remote: Vec<(String, u64)> = locked(&self.remote).keys().cloned().collect();
        for (from, sock) in remote {
            self.drop_remote(&from, sock);
        }
    }

    /// One heartbeat: refresh this replica's liveness and its ownership,
    /// release servers it no longer serves, and end sockets whose other
    /// replica is gone.
    async fn heartbeat(&self) {
        let Ok(mut conn) = self.pool.get().await else {
            // Without Redis no owner can be verified.
            let forwarded: Vec<u64> = locked(&self.forwarded).keys().copied().collect();
            for id in forwarded {
                self.close_forwarded(id, close::RESTART);
            }
            return;
        };
        let alive_ttl = self.timing.alive_ttl.as_millis() as u64;
        let _: Result<(), _> = redis::cmd("SET")
            .arg(alive_key(&self.replica))
            .arg("1")
            .arg("PX")
            .arg(alive_ttl)
            .query_async(&mut *conn)
            .await;

        let owned: Vec<SessionKey> = locked(&self.owned).iter().cloned().collect();
        for key in owned {
            if self.hub.has_session(&key) {
                let kept: Result<i64, _> = redis::Script::new(REFRESH)
                    .key(owner_key(&key))
                    .arg(&self.replica)
                    .arg(self.timing.owner_ttl.as_millis() as u64)
                    .invoke_async(&mut *conn)
                    .await;
                if matches!(kept, Ok(0)) {
                    // Another replica took the server: end this copy.
                    tracing::warn!("paseo relay lost ownership of a server; evicting");
                    locked(&self.owned).remove(&key);
                    locked(&self.remote).retain(|_, r| r.key != key);
                    self.hub.evict(&key);
                }
            } else {
                let released: Result<i64, _> = redis::Script::new(RELEASE)
                    .key(owner_key(&key))
                    .arg(&self.replica)
                    .invoke_async(&mut *conn)
                    .await;
                if released.is_ok() && !self.hub.has_session(&key) {
                    locked(&self.owned).remove(&key);
                }
            }
        }

        // Owner side: drop sockets of replicas that stopped refreshing.
        let origins: HashSet<String> = locked(&self.remote)
            .keys()
            .map(|(o, _)| o.clone())
            .collect();
        for origin in origins {
            let alive: Result<bool, _> = redis::cmd("EXISTS")
                .arg(alive_key(&origin))
                .query_async(&mut *conn)
                .await;
            if matches!(alive, Ok(false)) {
                let gone: Vec<u64> = locked(&self.remote)
                    .keys()
                    .filter(|(o, _)| *o == origin)
                    .map(|(_, s)| *s)
                    .collect();
                for sock in gone {
                    self.drop_remote(&origin, sock);
                }
            }
        }

        // Forwarding side: close sockets whose owner changed or died.
        let routes: HashMap<SessionKey, String> = locked(&self.forwarded)
            .values()
            .map(|f| (f.key.clone(), f.owner.clone()))
            .collect();
        for (key, owner) in routes {
            let current: Result<Option<String>, _> = redis::cmd("GET")
                .arg(owner_key(&key))
                .query_async(&mut *conn)
                .await;
            let alive: Result<bool, _> = redis::cmd("EXISTS")
                .arg(alive_key(&owner))
                .query_async(&mut *conn)
                .await;
            let healthy =
                matches!(&current, Ok(Some(o)) if *o == owner) && matches!(alive, Ok(true));
            if !healthy {
                let stale: Vec<u64> = locked(&self.forwarded)
                    .iter()
                    .filter(|(_, f)| f.key == key && f.owner == owner)
                    .map(|(id, _)| *id)
                    .collect();
                for id in stale {
                    self.close_forwarded(id, close::RESTART);
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn replica(&self) -> &str {
        &self.replica
    }
}

/// Run `task` until `stop` is cancelled.
fn spawn_until(
    stop: &CancellationToken,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let stop = stop.clone();
    tokio::spawn(async move {
        tokio::select! {
            () = stop.cancelled() => {}
            () = task => {}
        }
    });
}

/// Owner side: send a remote socket's outbound frames to its replica.
async fn relay_out(cluster: Weak<Cluster>, mut registered: Registered, origin: String, sock: u64) {
    while let Some(next) = registered.rx.recv().await {
        registered.release(&next);
        let Some(cluster) = cluster.upgrade() else {
            return;
        };
        let (envelope, last) = match next {
            Outbound::Frame(frame) => match Payload::from_message(&frame) {
                Some(payload) => (Envelope::Deliver { sock, payload }, false),
                None => continue,
            },
            Outbound::Close(code, _) => (Envelope::Close { sock, code }, true),
        };
        cluster.enqueue(
            &origin,
            &envelope,
            OnFailure::DropRemote(origin.clone(), sock),
        );
        if last {
            return;
        }
    }
    // The hub dropped the socket without a close (it stopped reading): close
    // it on its replica and forget it here.
    if let Some(cluster) = cluster.upgrade() {
        let id = registered.id;
        let removed = {
            let mut remote = locked(&cluster.remote);
            let key = (origin.clone(), sock);
            let current = remote.get(&key).is_some_and(|r| r.hub_id == id);
            if current {
                remote.remove(&key);
            }
            current
        };
        if removed {
            cluster.enqueue(
                &origin,
                &Envelope::Close {
                    sock,
                    code: close::TRY_AGAIN,
                },
                OnFailure::Nothing,
            );
        }
    }
}

/// Publish queued envelopes in order on one connection.
async fn publish_loop(
    cluster: Weak<Cluster>,
    redis_url: String,
    mut outgoing: mpsc::Receiver<Outgoing>,
) {
    let client = redis::Client::open(redis_url).ok();
    let mut conn: Option<redis::aio::MultiplexedConnection> = None;
    while let Some(out) = outgoing.recv().await {
        if conn.is_none() {
            if let Some(client) = &client {
                conn = client.get_multiplexed_async_connection().await.ok();
            }
        }
        let receivers = match &mut conn {
            Some(c) => redis::cmd("PUBLISH")
                .arg(&out.channel)
                .arg(&out.payload)
                .query_async::<i64>(c)
                .await
                .ok(),
            None => None,
        };
        let Some(cluster) = cluster.upgrade() else {
            return;
        };
        cluster.hub.bytes.release(out.payload.len());
        match receivers {
            Some(n) if n > 0 => {}
            Some(_) => {
                // No subscriber: the target replica is gone.
                cluster.fail(out.on_failure);
            }
            None => {
                tracing::warn!("paseo relay publish failed; closing the affected socket");
                conn = None;
                cluster.fail(out.on_failure);
            }
        }
    }
}

/// Subscribe to this replica's inbox, reconnecting with backoff.
async fn subscribe_loop(cluster: Weak<Cluster>, redis_url: String, replica: String) {
    let mut backoff = BACKOFF_INITIAL;
    loop {
        match subscribe_once(&cluster, &redis_url, &replica).await {
            Ok(()) => backoff = BACKOFF_INITIAL,
            Err(error) => tracing::error!("paseo relay inbox error: {error}"),
        }
        let Some(live) = cluster.upgrade() else {
            return;
        };
        live.subscribed.store(false, Ordering::Release);
        live.reset_cross_replica();
        drop(live);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn subscribe_once(
    cluster: &Weak<Cluster>,
    redis_url: &str,
    replica: &str,
) -> Result<(), redis::RedisError> {
    let client = redis::Client::open(redis_url)?;
    let mut pubsub = client.get_async_pubsub().await?;
    pubsub.subscribe(inbox(replica)).await?;
    match cluster.upgrade() {
        Some(live) => live.subscribed.store(true, Ordering::Release),
        None => return Ok(()),
    }
    let mut stream = pubsub.on_message();
    while let Some(message) = stream.next().await {
        let Some(live) = cluster.upgrade() else {
            return Ok(());
        };
        let Ok(payload) = message.get_payload::<String>() else {
            continue;
        };
        match serde_json::from_str::<Envelope>(&payload) {
            Ok(envelope) => live.handle(envelope),
            Err(error) => tracing::warn!("paseo relay dropped a malformed envelope: {error}"),
        }
    }
    Ok(())
}

async fn heartbeat_loop(cluster: Weak<Cluster>, every: Duration) {
    let mut ticker = tokio::time::interval(every);
    loop {
        ticker.tick().await;
        let Some(live) = cluster.upgrade() else {
            return;
        };
        live.heartbeat().await;
    }
}

#[cfg(test)]
impl Cluster {
    /// Stop this replica's tasks as if its process died: no more heartbeats
    /// or inbox. Its alive key then expires.
    pub(super) fn halt(&self) {
        self.stop.cancel();
    }

    pub(super) fn is_subscribed(&self) -> bool {
        self.subscribed.load(Ordering::Acquire)
    }
}

#[cfg(test)]
#[path = "cluster_tests.rs"]
mod tests;
