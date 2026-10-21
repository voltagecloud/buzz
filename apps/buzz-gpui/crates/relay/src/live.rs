//! Live traffic for one community: which subscriptions exist and how they
//! recover (`live.ts`, spec §3.3–3.6).
//!
//! A *route* is a logical interest: the global `profiles` and `membership`
//! feeds, presence, the agent observer, a single channel, or a batch of up
//! to ten joined channels. A *wire* is the REQ currently carrying a route;
//! replacing a wire keeps the established one open until the new one reaches
//! EOSE, so no events are missed while a batch changes shape.
//!
//! Every attempt to connect rebuilds the routes with a fresh five-minute
//! look-back rather than resuming; the session follows up with HTTP
//! catch-up reads for anything older. Nothing is dispatched until the
//! socket has authenticated, and a shared quota cooldown gates every
//! dispatch.

use crate::event::{Verified, Verifier, is_hex64, kinds};
use crate::origin::RelayOrigin;
use crate::socket::{
    Cooldown, Filter, RelaySocket, SocketOptions, SocketState, SubscriptionEvent, filter,
    quota_hint,
};
use buzz_identity::Identity;
use futures::StreamExt;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_stream::StreamMap;

#[derive(Clone, Debug)]
pub struct LiveOptions {
    pub batch_size: usize,
    pub channel_capacity: usize,
    pub replay_limit: u64,
    pub lookback: Duration,
    pub setup_concurrency: usize,
    pub setup_deadline: Duration,
    pub recovery_interval: Duration,
    pub max_quota_retries: u32,
    pub socket: SocketOptions,
}

impl Default for LiveOptions {
    fn default() -> Self {
        Self {
            batch_size: 10,
            channel_capacity: 1022,
            replay_limit: 500,
            lookback: Duration::from_secs(300),
            setup_concurrency: 4,
            setup_deadline: Duration::from_secs(10),
            recovery_interval: Duration::from_secs(60),
            max_quota_retries: 3,
            socket: SocketOptions {
                resubscribe: false,
                ..Default::default()
            },
        }
    }
}

/// Most channel interests the session may name (`liveChannels`).
pub const MAX_INTERESTS: usize = 1024;
/// Most people whose presence is followed live.
pub const MAX_PRESENCE_AUTHORS: usize = 256;
/// Prioritized channels set up first.
pub const MAX_PRIORITIZED: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RouteKey {
    Profiles,
    Membership,
    Presence,
    /// Agent activity and turn metrics addressed to the viewer, while the
    /// archive captures them.
    Observer,
    Channel(String),
    Batch(u64),
}

impl RouteKey {
    fn global(&self) -> bool {
        !matches!(self, Self::Channel(_) | Self::Batch(_))
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Profiles => "profiles",
            Self::Membership => "membership",
            Self::Presence => "presence",
            Self::Observer => "observer",
            Self::Channel(_) | Self::Batch(_) => "channel",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Stored events sent before EOSE.
    Replay,
    Live,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteStatus {
    /// Waiting for its turn, a cooldown or the connection.
    Pending,
    /// Sent, waiting for EOSE.
    SettingUp,
    Live,
    Error(String),
    /// Over the channel capacity; finite reads still work.
    Limited,
}

/// The connection as the user sees it (`LiveSnapshot.status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveConnection {
    Connecting,
    Connected,
    /// Lost; reconnecting by itself.
    Retrying,
    /// Stopped until the user retries.
    Error,
}

/// What the replay before EOSE proves. EOSE establishes a stream; it never
/// proves the history complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    Unknown,
    /// The replay hit its limit: older live traffic may be missing.
    Limited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveRouteStatus {
    Pending,
    Live,
    Error,
    Limited,
}

/// One route as the session reports it: one entry per channel, so a batch
/// shows up as its channels (`LiveRoute`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveRoute {
    /// `profiles`, `membership`, `presence`, `observer` or `channel:<id>`.
    pub id: String,
    pub channel: Option<String>,
    pub status: LiveRouteStatus,
    pub replay: Replay,
    /// The last failure; kept while a retry is pending, until EOSE.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSnapshot {
    pub status: LiveConnection,
    pub routes: Vec<LiveRoute>,
    pub error: Option<String>,
    /// While [`LiveConnection::Retrying`]: the wait before the next attempt,
    /// which grows the longer the connection stays lost.
    pub retry_in: Option<Duration>,
}

impl Default for LiveSnapshot {
    fn default() -> Self {
        Self {
            status: LiveConnection::Connecting,
            routes: Vec::new(),
            error: None,
            retry_in: None,
        }
    }
}

#[derive(Debug)]
pub enum LiveUpdate {
    /// Verified events with their provenance (`LiveProvenance`).
    Events {
        events: Vec<Verified>,
        phase: Phase,
        /// The one channel this traffic belongs to, when unambiguous.
        channel: Option<String>,
        /// The carrying wire's channels, for traffic with no single channel.
        /// Never alert attribution.
        sources: Arc<[String]>,
    },
    Presence(Vec<Verified>),
    /// Agent activity (24200) or turn metrics (44200) for the viewer.
    Observer(Vec<Verified>),
    /// A route reached EOSE. For channel routes the session catches up
    /// those channels over HTTP; for globals it refreshes global state.
    Established {
        route: RouteKey,
        channels: Vec<String>,
    },
    /// `restricted: not a channel member`: access to these channels is gone.
    Denied {
        channels: Vec<String>,
        reason: String,
    },
    /// `restricted: channel access revoked`: a hint to re-read the roster,
    /// not proof of removal.
    AccessHint {
        channels: Vec<String>,
    },
    Connected,
    /// The socket dropped; everything live is stale until `Connected`.
    Disconnected,
    /// The minute tick: a repair hint (roster and unread), not evidence.
    Recover,
    /// The snapshot changed; read it with [`Live::snapshot`].
    Status,
}

struct Route {
    key: RouteKey,
    channels: Vec<String>,
    since: u64,
    /// The archive's metrics floor across a renewal (`metricsSince`).
    metrics_since: Option<u64>,
    status: RouteStatus,
    /// Kept through a pending retry until EOSE.
    error: Option<String>,
    replay: Replay,
    wire: Option<u64>,
    /// The established wire being replaced, kept until `wire` reaches EOSE.
    previous: Option<u64>,
    /// A failed renewal kept `previous`; the minute tick tries again.
    retry_renewal: bool,
    setup_deadline: Option<Instant>,
    quota_retries: u32,
    /// Events received before EOSE.
    count: u64,
    /// Replacement wires ask only for new events.
    live_only: bool,
    /// Insertion order, for ties in setup order.
    order: u64,
}

struct Wire {
    route: RouteKey,
    scope: Arc<[String]>,
    /// Reached EOSE: an established stream a failed renewal may keep.
    established: bool,
}

pub struct Live {
    socket: RelaySocket,
    socket_state: watch::Receiver<SocketState>,
    cooldown: Cooldown,
    viewer: String,
    options: LiveOptions,
    connected: bool,
    connection: LiveConnection,
    connection_error: Option<String>,
    retry_in: Option<Duration>,
    /// Invalid input from the session, shown until the next valid update.
    input_error: Option<String>,
    routes: BTreeMap<RouteKey, Route>,
    wires: HashMap<u64, Wire>,
    streams: StreamMap<u64, crate::socket::Subscription>,
    next_wire: u64,
    next_batch: u64,
    next_order: u64,
    /// Inputs, kept so routes can be rebuilt after a reconnect.
    interests: Vec<String>,
    joined: HashSet<String>,
    prioritized: Vec<String>,
    presence_authors: Vec<String>,
    observer_kinds: Vec<u16>,
    recovery: Option<Instant>,
    outbox: VecDeque<LiveUpdate>,
}

impl Live {
    pub fn connect(
        runtime: &tokio::runtime::Handle,
        origin: RelayOrigin,
        identity: Arc<Identity>,
        verifier: Arc<Verifier>,
        options: LiveOptions,
    ) -> Self {
        let viewer = identity.pubkey_hex();
        let socket_options = SocketOptions {
            resubscribe: false,
            ..options.socket.clone()
        };
        let cooldown = socket_options.cooldown.clone();
        let socket = RelaySocket::connect(runtime, origin, identity, verifier, socket_options);
        let mut live = Self {
            socket_state: socket.state(),
            socket,
            cooldown,
            viewer,
            options,
            connected: false,
            connection: LiveConnection::Connecting,
            connection_error: None,
            retry_in: None,
            input_error: None,
            routes: BTreeMap::new(),
            wires: HashMap::new(),
            streams: StreamMap::new(),
            next_wire: 1,
            next_batch: 1,
            next_order: 0,
            interests: Vec::new(),
            joined: HashSet::new(),
            prioritized: Vec::new(),
            presence_authors: Vec::new(),
            observer_kinds: Vec::new(),
            recovery: None,
            outbox: VecDeque::new(),
        };
        live.sync();
        live
    }

    pub fn socket(&self) -> &RelaySocket {
        &self.socket
    }

    /// The quota cooldown shared by every dispatch on this connection.
    pub fn cooldown(&self) -> &Cooldown {
        &self.cooldown
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn statuses(&self) -> Vec<(RouteKey, RouteStatus)> {
        self.routes
            .values()
            .map(|route| (route.key.clone(), route.status.clone()))
            .collect()
    }

    /// The connection and every route, a batch expanded to its channels.
    pub fn snapshot(&self) -> LiveSnapshot {
        let mut routes: Vec<&Route> = self.routes.values().collect();
        routes.sort_by_key(|route| route.order);
        let routes = routes
            .into_iter()
            .flat_map(|route| {
                let status = match route.status {
                    RouteStatus::Pending | RouteStatus::SettingUp => LiveRouteStatus::Pending,
                    RouteStatus::Live => LiveRouteStatus::Live,
                    RouteStatus::Error(_) => LiveRouteStatus::Error,
                    RouteStatus::Limited => LiveRouteStatus::Limited,
                };
                let entry = |channel: Option<&String>| LiveRoute {
                    id: channel.map_or_else(
                        || route.key.name().to_string(),
                        |id| format!("channel:{id}"),
                    ),
                    channel: channel.cloned(),
                    status,
                    replay: route.replay,
                    error: route.error.clone(),
                };
                if route.key.global() {
                    vec![entry(None)]
                } else {
                    route.channels.iter().map(|id| entry(Some(id))).collect()
                }
            })
            .collect();
        LiveSnapshot {
            status: if self.input_error.is_some() {
                LiveConnection::Error
            } else {
                self.connection
            },
            routes,
            error: self
                .input_error
                .clone()
                .or_else(|| self.connection_error.clone()),
            retry_in: self.retry_in,
        }
    }

    /// Sets which channels need live traffic. `joined` channels may share
    /// batched routes; `prioritized` ones (open or previewed, at most 64)
    /// get their own routes and are set up first. Interests are bounded
    /// IDs, never truncated: invalid input is refused whole.
    pub fn update(
        &mut self,
        interests: impl IntoIterator<Item = String>,
        joined: impl IntoIterator<Item = String>,
        prioritized: Vec<String>,
    ) -> Result<(), String> {
        let checked = (|| {
            let interests = live_channels(interests.into_iter().collect())?;
            let joined = live_channels(joined.into_iter().collect())?;
            if joined.iter().any(|id| interests.binary_search(id).is_err()) {
                return Err("Joined scope outside live interests".to_string());
            }
            live_channels(prioritized.clone())?;
            let mut seen = HashSet::new();
            let prioritized: Vec<String> = prioritized
                .into_iter()
                .filter(|id| seen.insert(id.clone()))
                .take(MAX_PRIORITIZED)
                .collect();
            Ok((interests, joined, prioritized))
        })();
        let (interests, joined, prioritized) = match checked {
            Ok(input) => input,
            Err(error) => {
                self.input_error = Some(error.clone());
                self.outbox.push_back(LiveUpdate::Status);
                return Err(error);
            }
        };
        let joined: HashSet<String> = joined.into_iter().collect();
        let cleared = self.input_error.take().is_some();
        if interests == self.interests && joined == self.joined && prioritized == self.prioritized {
            if cleared {
                self.outbox.push_back(LiveUpdate::Status);
            }
            return Ok(());
        }
        self.interests = interests;
        self.joined = joined;
        self.prioritized = prioritized;
        self.sync();
        Ok(())
    }

    /// Authors whose presence to follow live (at most 256 lowercase hex
    /// keys; anything else is refused whole).
    pub fn watch_presence(&mut self, authors: Vec<String>) -> Result<(), String> {
        if authors.len() > MAX_PRESENCE_AUTHORS || authors.iter().any(|key| !is_hex64(key)) {
            let error = "Invalid live presence authors".to_string();
            self.input_error = Some(error.clone());
            self.outbox.push_back(LiveUpdate::Status);
            return Err(error);
        }
        let mut authors = authors;
        authors.sort();
        authors.dedup();
        if authors == self.presence_authors {
            return Ok(());
        }
        self.presence_authors = authors;
        if self.routes.contains_key(&RouteKey::Presence) {
            if self.presence_authors.is_empty() {
                self.remove_route(&RouteKey::Presence);
            } else {
                self.replace(&RouteKey::Presence, None);
            }
        }
        self.sync();
        Ok(())
    }

    /// Agent kinds to receive live while they are archived; empty stops
    /// the route (`archive`, relay.md §3.3). Only kinds 24200 and 44200.
    pub fn watch_observer(&mut self, mut kinds: Vec<u16>) {
        kinds.retain(|kind| matches!(*kind, kinds::AGENT_ACTIVITY | kinds::AGENT_METRICS));
        kinds.sort_unstable();
        kinds.dedup();
        if kinds == self.observer_kinds {
            return;
        }
        let keep_metrics = self.observer_kinds.contains(&kinds::AGENT_METRICS)
            && kinds.contains(&kinds::AGENT_METRICS);
        self.observer_kinds = kinds;
        if self.routes.contains_key(&RouteKey::Observer) {
            self.renew_telemetry(keep_metrics);
        }
        self.sync();
    }

    /// Renews the observer route for new kinds. Stored metrics can bridge
    /// the gap; ephemeral activity cannot, and nothing older than a minute
    /// is replayed.
    fn renew_telemetry(&mut self, keep_metrics: bool) {
        if !keep_metrics {
            self.remove_route(&RouteKey::Observer);
            return;
        }
        let Some(route) = self.routes.get_mut(&RouteKey::Observer) else {
            return;
        };
        if route.wire.is_some() {
            route.metrics_since = Some(
                route
                    .metrics_since
                    .unwrap_or_else(|| route.since.max(now_secs().saturating_sub(60))),
            );
        }
        let wires: Vec<u64> = route
            .wire
            .take()
            .into_iter()
            .chain(route.previous.take())
            .collect();
        route.status = RouteStatus::Pending;
        route.setup_deadline = None;
        route.count = 0;
        route.quota_retries = 0;
        route.error = None;
        route.retry_renewal = false;
        for wire in wires {
            self.close_wire(wire);
        }
    }

    /// The user's "Retry live updates": a fresh reconnect budget, and
    /// failed routes get another go. The relay's cooldown still applies.
    pub fn retry(&mut self) {
        if self.connected {
            for route in self.routes.values_mut() {
                if matches!(route.status, RouteStatus::Error(_)) {
                    route.status = RouteStatus::Pending;
                    route.count = 0;
                    route.quota_retries = 0;
                }
            }
            self.pump();
            self.outbox.push_back(LiveUpdate::Status);
        } else {
            self.socket.reconnect();
        }
    }

    /// Waits for the next update. Cancel-safe: call it in a `select!`.
    pub async fn next(&mut self) -> LiveUpdate {
        loop {
            if let Some(update) = self.outbox.pop_front() {
                return update;
            }
            let setup = self
                .routes
                .values()
                .filter_map(|route| route.setup_deadline)
                .min();
            let pending = self
                .routes
                .values()
                .any(|route| route.status == RouteStatus::Pending);
            let cooldown = self.cooldown.until().filter(|_| pending && self.connected);
            let recovery = self.recovery.filter(|_| self.connected);
            tokio::select! {
                changed = self.socket_state.changed() => {
                    if changed.is_err() {
                        return LiveUpdate::Disconnected;
                    }
                    let state = self.socket_state.borrow_and_update().clone();
                    self.connection_changed(state);
                }
                Some((wire, event)) = self.streams.next(), if !self.streams.is_empty() => {
                    self.wire_event(wire, event);
                }
                _ = sleep_until(setup) => self.expire_setups(),
                _ = sleep_until(cooldown) => self.pump(),
                _ = sleep_until(recovery) => self.recover(),
            }
        }
    }

    fn connection_changed(&mut self, state: SocketState) {
        let was_connected = self.connected;
        self.connected = state == SocketState::Ready;
        self.retry_in = None;
        match state {
            SocketState::Ready => {
                self.connection = LiveConnection::Connected;
                self.connection_error = None;
            }
            SocketState::Connecting { .. } | SocketState::Authenticating => {
                self.connection = LiveConnection::Connecting;
                self.connection_error = None;
            }
            SocketState::Waiting { error, retry_in } => {
                self.connection = LiveConnection::Retrying;
                self.connection_error = Some(error);
                self.retry_in = Some(retry_in);
            }
            SocketState::Failed { error } => {
                self.connection = LiveConnection::Error;
                self.connection_error = Some(error);
            }
        }
        if self.connected && !was_connected || !self.connected {
            // Rebuilt from inputs with a fresh look-back for every
            // connection; pending while there is none.
            self.routes.clear();
            self.wires.clear();
            self.streams = StreamMap::new();
            self.sync();
        }
        if self.connected && !was_connected {
            self.recovery = Some(Instant::now() + self.options.recovery_interval);
            self.outbox.push_back(LiveUpdate::Connected);
        } else if was_connected && !self.connected {
            self.recovery = None;
            self.outbox.push_back(LiveUpdate::Disconnected);
        }
        self.outbox.push_back(LiveUpdate::Status);
    }

    fn scope(route: &Route) -> &[String] {
        &route.channels
    }

    /// Reconciles routes with the inputs (`sync`). A wire's scope never
    /// changes: a batch that loses channels is replaced, and navigation only
    /// reorders setup; it never rebalances healthy subscriptions.
    fn sync(&mut self) {
        let mut ranked: Vec<&String> = self
            .prioritized
            .iter()
            .filter(|id| self.interests.binary_search(id).is_ok())
            .collect();
        let mut seen: HashSet<&String> = ranked.iter().copied().collect();
        ranked.extend(self.interests.iter().filter(|id| seen.insert(id)));
        let capacity = self
            .options
            .channel_capacity
            .saturating_sub(usize::from(!self.observer_kinds.is_empty()))
            .saturating_sub(usize::from(!self.presence_authors.is_empty()));
        let admitted: HashSet<String> = ranked.into_iter().take(capacity).cloned().collect();
        let mut remaining: HashSet<String> = self.interests.iter().cloned().collect();
        let mut globals: Vec<RouteKey> = vec![RouteKey::Profiles, RouteKey::Membership];
        if !self.observer_kinds.is_empty() {
            globals.push(RouteKey::Observer);
        }
        if !self.presence_authors.is_empty() {
            globals.push(RouteKey::Presence);
        }

        let mut keys: Vec<(u64, RouteKey)> = self
            .routes
            .values()
            .map(|route| (route.order, route.key.clone()))
            .collect();
        keys.sort();
        for (_, key) in keys {
            let route = &self.routes[&key];
            let ids = Self::scope(route).to_vec();
            let batch = matches!(key, RouteKey::Batch(_));
            let limited = route.status == RouteStatus::Limited;
            let stale = if ids.is_empty() {
                !globals.contains(&key)
            } else {
                ids.iter().any(|id| {
                    !remaining.contains(id)
                        || limited == admitted.contains(id)
                        || (batch && !self.joined.contains(id))
                })
            };
            if stale {
                let survivors: Vec<String> = ids
                    .iter()
                    .filter(|id| {
                        remaining.contains(*id)
                            && admitted.contains(*id)
                            && self.joined.contains(*id)
                    })
                    .cloned()
                    .collect();
                if batch
                    && !survivors.is_empty()
                    && (route.status == RouteStatus::Live || route.live_only)
                {
                    for id in &survivors {
                        remaining.remove(id);
                    }
                    self.replace(&key, Some(survivors));
                } else {
                    self.remove_route(&key);
                }
            } else {
                for id in &ids {
                    remaining.remove(id);
                }
                globals.retain(|global| *global != key);
            }
        }
        for key in globals {
            let live_only = key == RouteKey::Presence;
            self.add_route(key, Vec::new()).live_only = live_only;
        }
        // In interest order.
        let remaining: Vec<String> = self
            .interests
            .iter()
            .filter(|id| remaining.contains(*id))
            .cloned()
            .collect();
        let mut batch = Vec::new();
        for id in remaining {
            if !admitted.contains(&id) {
                let route = self.add_route(RouteKey::Channel(id.clone()), vec![id]);
                route.status = RouteStatus::Limited;
                route.error =
                    Some("Live channel capacity reached; finite reads remain available".into());
            } else if !self.joined.contains(&id) || self.prioritized.contains(&id) {
                self.add_route(RouteKey::Channel(id.clone()), vec![id]);
            } else {
                batch.push(id);
            }
        }
        for chunk in batch.chunks(self.options.batch_size) {
            let key = RouteKey::Batch(self.next_batch);
            self.next_batch += 1;
            self.add_route(key, chunk.to_vec());
        }
        self.pump();
        self.outbox.push_back(LiveUpdate::Status);
    }

    fn add_route(&mut self, key: RouteKey, channels: Vec<String>) -> &mut Route {
        self.next_order += 1;
        let route = Route {
            key: key.clone(),
            channels,
            since: now_secs().saturating_sub(self.options.lookback.as_secs()),
            metrics_since: None,
            status: RouteStatus::Pending,
            error: None,
            replay: Replay::Unknown,
            wire: None,
            previous: None,
            retry_renewal: false,
            setup_deadline: None,
            quota_retries: 0,
            count: 0,
            live_only: false,
            order: self.next_order,
        };
        self.routes.insert(key.clone(), route);
        self.routes.get_mut(&key).expect("route was just added")
    }

    fn remove_route(&mut self, key: &RouteKey) {
        if let Some(route) = self.routes.remove(key) {
            for wire in route.wire.into_iter().chain(route.previous) {
                self.close_wire(wire);
            }
        }
    }

    fn close_wire(&mut self, wire: u64) {
        // Fenced first so late frames are ignored; dropping the
        // subscription sends CLOSE.
        self.wires.remove(&wire);
        self.streams.remove(&wire);
    }

    /// Re-sends a route on a fresh wire asking only for new events. The
    /// established wire stays until the new one reaches EOSE; a replacement
    /// that was still setting up is retired instead, so at most two wires
    /// carry a route.
    fn replace(&mut self, key: &RouteKey, channels: Option<Vec<String>>) {
        let Some(route) = self.routes.get_mut(key) else {
            return;
        };
        let retire = if route.previous.is_some() {
            route.wire.take()
        } else {
            route.previous = route.wire.take();
            None
        };
        if let Some(channels) = channels {
            route.channels = channels;
        }
        route.status = RouteStatus::Pending;
        route.live_only = true;
        route.count = 0;
        route.quota_retries = 0;
        route.setup_deadline = None;
        route.retry_renewal = false;
        if let Some(wire) = retire {
            self.close_wire(wire);
        }
    }

    fn filters(&self, route: &Route) -> Vec<Filter> {
        let limit = if route.live_only {
            0
        } else {
            self.options.replay_limit
        };
        let since = route.metrics_since.unwrap_or(route.since);
        let channel = |ids: Vec<&String>| {
            filter(json!({
                "kinds": kinds::CHANNEL_KINDS,
                "#h": ids,
                "since": since,
                "limit": limit,
            }))
        };
        let replay = self.options.replay_limit;
        match &route.key {
            RouteKey::Profiles => vec![
                filter(
                    json!({"kinds": [kinds::PROFILE, kinds::AGENT_METADATA, kinds::MANAGED_AGENT], "since": since, "limit": limit}),
                ),
                filter(
                    json!({"kinds": [kinds::USER_STATUS], "#d": ["general"], "since": route.since, "limit": replay}),
                ),
                filter(
                    json!({"kinds": [kinds::CUSTOM_EMOJI], "#d": ["buzz:custom-emoji"], "since": route.since, "limit": replay}),
                ),
            ],
            RouteKey::Membership => vec![
                filter(
                    json!({"kinds": [kinds::MEMBER_ADDED, kinds::MEMBER_REMOVED], "#p": [self.viewer], "since": since, "limit": limit}),
                ),
                filter(
                    json!({"kinds": [kinds::APP_DATA], "authors": [self.viewer], "#t": ["read-state"], "since": route.since, "limit": replay}),
                ),
                filter(
                    json!({"kinds": [kinds::REMINDERS], "authors": [self.viewer], "since": route.since, "limit": replay}),
                ),
            ],
            RouteKey::Presence => vec![filter(json!({
                "kinds": [kinds::PRESENCE],
                "authors": self.presence_authors,
                "since": since,
                "limit": limit,
            }))],
            // Without a limit: the archive only takes fresh envelopes.
            RouteKey::Observer => vec![filter(json!({
                "kinds": self.observer_kinds,
                "#p": [self.viewer],
                "since": since,
            }))],
            // One filter per channel on a first setup, so each keeps its own
            // replay allowance; replacements name every channel at once.
            RouteKey::Channel(_) | RouteKey::Batch(_) if route.live_only => {
                vec![channel(route.channels.iter().collect())]
            }
            RouteKey::Channel(_) | RouteKey::Batch(_) => {
                route.channels.iter().map(|id| channel(vec![id])).collect()
            }
        }
    }

    /// Starts pending routes once authenticated: globals first, then by
    /// priority, at most `setup_concurrency` awaiting EOSE, none while the
    /// relay's cooldown runs.
    fn pump(&mut self) {
        if !self.connected {
            return;
        }
        let rank = |route: &Route, prioritized: &[String]| -> (i64, u64) {
            let scope = Self::scope(route);
            let rank = if scope.is_empty() {
                -2
            } else {
                scope
                    .iter()
                    .map(|id| {
                        prioritized
                            .iter()
                            .position(|p| p == id)
                            .unwrap_or(prioritized.len()) as i64
                    })
                    .min()
                    .unwrap_or(prioritized.len() as i64)
            };
            (rank, route.order)
        };
        let active = self
            .routes
            .values()
            .filter(|route| route.status == RouteStatus::SettingUp)
            .count();
        let mut order: Vec<((i64, u64), RouteKey)> = self
            .routes
            .values()
            .filter(|route| route.status == RouteStatus::Pending)
            .map(|route| (rank(route, &self.prioritized), route.key.clone()))
            .collect();
        order.sort();
        let room = self.options.setup_concurrency.saturating_sub(active);
        for (_, key) in order.into_iter().take(room) {
            if self.cooldown.until().is_some() {
                return;
            }
            let wire = self.next_wire;
            self.next_wire += 1;
            let route = self.routes.get_mut(&key).expect("pending route exists");
            if key == RouteKey::Observer {
                route.since = now_secs();
                route.metrics_since = route
                    .metrics_since
                    .map(|floor| floor.max(route.since.saturating_sub(240)));
            }
            route.wire = Some(wire);
            route.status = RouteStatus::SettingUp;
            route.setup_deadline = Some(Instant::now() + self.options.setup_deadline);
            let scope: Arc<[String]> = route.channels.clone().into();
            let filters = self.filters(&self.routes[&key]);
            let subscription = self.socket.subscribe(filters);
            self.wires.insert(
                wire,
                Wire {
                    route: key,
                    scope,
                    established: false,
                },
            );
            self.streams.insert(wire, subscription);
        }
    }

    fn wire_event(&mut self, wire: u64, event: SubscriptionEvent) {
        let Some(state) = self.wires.get(&wire) else {
            return;
        };
        let key = state.route.clone();
        let wire_scope = state.scope.clone();
        let Some(route) = self.routes.get(&key) else {
            return;
        };
        let current = route.wire == Some(wire);
        let previous = route.previous == Some(wire);
        if !current && !previous {
            return;
        }
        if previous
            && !matches!(
                event,
                SubscriptionEvent::Event(_) | SubscriptionEvent::Invalid
            )
        {
            // Only the previous wire's own end matters; its EOSE does not.
            if matches!(
                event,
                SubscriptionEvent::Closed(_) | SubscriptionEvent::Lost
            ) {
                self.close_wire(wire);
                if let Some(route) = self.routes.get_mut(&key) {
                    route.previous = None;
                }
            }
            return;
        }
        match event {
            SubscriptionEvent::Event(event) => self.deliver(&key, &wire_scope, previous, event),
            SubscriptionEvent::Invalid => {
                self.fail(&key, "Relay supplied invalid live traffic".into(), false);
            }
            SubscriptionEvent::Eose => {
                let route = self.routes.get_mut(&key).expect("current route exists");
                if route.status != RouteStatus::SettingUp {
                    return;
                }
                if let Some(state) = self.wires.get_mut(&wire) {
                    state.established = true;
                }
                route.status = RouteStatus::Live;
                route.setup_deadline = None;
                route.metrics_since = None;
                route.error = None;
                route.replay = if route.count >= self.options.replay_limit {
                    Replay::Limited
                } else {
                    Replay::Unknown
                };
                let channels = route.channels.clone();
                if let Some(previous) = route.previous.take() {
                    self.close_wire(previous);
                }
                self.outbox.push_back(LiveUpdate::Status);
                if !matches!(key, RouteKey::Presence | RouteKey::Observer) {
                    self.outbox.push_back(LiveUpdate::Established {
                        route: key,
                        channels,
                    });
                }
                self.pump();
            }
            SubscriptionEvent::Closed(reason) => {
                let transient = reason.starts_with("rate-limited:") || reason.starts_with("error:");
                self.fail(&key, reason, transient);
            }
            SubscriptionEvent::Lost | SubscriptionEvent::Resubscribed => {}
        }
    }

    /// Attributes and forwards one event (`onmessage` EVENT).
    fn deliver(&mut self, key: &RouteKey, ids: &Arc<[String]>, previous: bool, event: Verified) {
        let Some(route) = self.routes.get_mut(key) else {
            return;
        };
        let h: Vec<&String> = event.tags_named("h").filter_map(|tag| tag.get(1)).collect();
        let destination = match h.as_slice() {
            [one] => Some((*one).clone()),
            [] if matches!(
                event.kind_u16(),
                kinds::CHANNEL_METADATA | kinds::CHANNEL_ADMINS | kinds::CHANNEL_MEMBERS
            ) =>
            {
                event.tag("d").map(str::to_string)
            }
            _ => None,
        };
        let channel = match destination {
            Some(id) if ids.contains(&id) => Some(id),
            _ if h.is_empty() && matches!(key, RouteKey::Channel(_)) => ids.first().cloned(),
            _ => None,
        };
        if !ids.is_empty() && !h.is_empty() && !h.iter().any(|id| ids.contains(id)) {
            return;
        }
        if previous {
            // The old wire's traffic must fit the narrowed scope; untagged
            // auxiliaries cannot inherit it, so the replacement delivers
            // fresh copies instead.
            let retained = &route.channels;
            let outside = match &channel {
                Some(id) => !retained.contains(id),
                None => ids.iter().any(|id| !retained.contains(id)),
            };
            if outside {
                return;
            }
        }
        if event.kind_u16() == kinds::TYPING && channel.is_none() {
            return;
        }
        if !previous && route.status == RouteStatus::SettingUp && !route.live_only {
            route.count += 1;
        }
        let phase = if route.live_only || route.status == RouteStatus::Live {
            Phase::Live
        } else {
            Phase::Replay
        };
        match key {
            RouteKey::Presence => {
                if event.kind_u16() == kinds::PRESENCE
                    && self.presence_authors.contains(&event.pubkey_hex())
                {
                    self.outbox.push_back(LiveUpdate::Presence(vec![event]));
                }
            }
            RouteKey::Observer => {
                let floor = if event.kind_u16() == kinds::AGENT_METRICS {
                    route.metrics_since.unwrap_or(route.since)
                } else {
                    route.since
                };
                if self.observer_kinds.contains(&event.kind_u16())
                    && event.created_at_secs() >= floor
                    && event
                        .tags_named("p")
                        .any(|tag| tag.get(1) == Some(&self.viewer))
                {
                    self.outbox.push_back(LiveUpdate::Observer(vec![event]));
                }
            }
            _ => {
                if matches!(
                    event.kind_u16(),
                    kinds::AGENT_ACTIVITY | kinds::AGENT_METRICS
                ) {
                    return;
                }
                self.outbox.push_back(LiveUpdate::Events {
                    events: vec![event],
                    phase,
                    sources: if channel.is_none() {
                        ids.clone()
                    } else {
                        Arc::new([])
                    },
                    channel,
                });
            }
        }
    }

    /// A route's wire failed (`fail`). Only a transient failure of an
    /// unchanged, established scope keeps the old wire; quota refusals
    /// pause every dispatch and retry quietly a few times.
    fn fail(&mut self, key: &RouteKey, reason: String, transient: bool) {
        let max_retries = self.options.max_quota_retries;
        let Some(route) = self.routes.get_mut(key) else {
            return;
        };
        let previous = route.previous;
        let retain = transient
            && previous.is_some_and(|wire| {
                self.wires
                    .get(&wire)
                    .is_some_and(|w| w.established && *w.scope == *route.channels)
            });
        let mut closing: Vec<u64> = route.wire.take().into_iter().collect();
        if retain {
            route.retry_renewal = !reason.starts_with("rate-limited:");
        } else {
            closing.extend(route.previous.take());
            route.retry_renewal = false;
        }
        route.setup_deadline = None;
        route.status = RouteStatus::Error(reason.clone());
        route.error = Some(reason.clone());
        let channels = route.channels.clone();
        let mut stop_unsent = None;
        if reason.starts_with("rate-limited:") {
            let seconds = quota_hint(&reason).unwrap_or(5);
            if seconds > 60 {
                stop_unsent = Some("Unsupported live cooldown; automatic setup stopped");
                self.cooldown
                    .pause(seconds.min(crate::socket::MAX_COOLDOWN_SECS));
            } else {
                self.cooldown.pause(seconds);
                route.quota_retries += 1;
                if route.quota_retries <= max_retries {
                    route.status = RouteStatus::Pending;
                } else {
                    stop_unsent = Some("Live request cooldown retries exhausted; retry available");
                }
            }
        }
        for wire in closing {
            self.close_wire(wire);
        }
        if let Some(error) = stop_unsent {
            // Never drain the unsent queue into a cooldown already refused.
            for queued in self.routes.values_mut() {
                if queued.status == RouteStatus::Pending && queued.wire.is_none() {
                    queued.status = RouteStatus::Error(error.into());
                    queued.error = Some(error.into());
                }
            }
        }
        if reason == "restricted: not a channel member" {
            self.outbox
                .push_back(LiveUpdate::Denied { channels, reason });
        } else if reason.starts_with("restricted: channel access revoked") {
            self.outbox.push_back(LiveUpdate::AccessHint { channels });
        }
        self.outbox.push_back(LiveUpdate::Status);
        self.pump();
    }

    fn expire_setups(&mut self) {
        let now = Instant::now();
        let expired: Vec<RouteKey> = self
            .routes
            .values()
            .filter(|route| route.setup_deadline.is_some_and(|deadline| deadline <= now))
            .map(|route| route.key.clone())
            .collect();
        for key in expired {
            self.fail(
                &key,
                "Live subscription setup timed out; retry available".into(),
                true,
            );
        }
    }

    /// The minute tick: renews healthy batches on fresh wires so a relay
    /// that silently pruned a subscription is noticed, retries failed
    /// renewals that kept their wire, and asks the session to repair.
    fn recover(&mut self) {
        self.recovery = Some(Instant::now() + self.options.recovery_interval);
        if !self.connected {
            return;
        }
        let keys: Vec<RouteKey> = self.routes.keys().cloned().collect();
        for key in keys {
            let route = &self.routes[&key];
            if matches!(key, RouteKey::Batch(_))
                && route.status == RouteStatus::Live
                && route.previous.is_none()
            {
                self.replace(&key, None);
            } else if matches!(route.status, RouteStatus::Error(_))
                && route.retry_renewal
                && route
                    .previous
                    .is_some_and(|wire| self.wires.contains_key(&wire))
            {
                let route = self.routes.get_mut(&key).expect("route exists");
                route.status = RouteStatus::Pending;
                route.retry_renewal = false;
            }
        }
        self.pump();
        self.outbox.push_back(LiveUpdate::Status);
        self.outbox.push_back(LiveUpdate::Recover);
    }
}

/// Bounded channel IDs, deduplicated and sorted (`liveChannels`).
fn live_channels(mut ids: Vec<String>) -> Result<Vec<String>, String> {
    let valid = |id: &String| {
        (1..=128).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    if ids.len() > MAX_INTERESTS || !ids.iter().all(valid) {
        return Err("Invalid live channel interests (maximum 1024 bounded IDs)".into());
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => futures::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interests_are_bounded_ids_never_truncated() {
        assert_eq!(
            live_channels(vec!["b".into(), "a".into(), "b".into()]),
            Ok(vec!["a".into(), "b".into()])
        );
        assert!(live_channels(vec!["has space".into()]).is_err());
        assert!(live_channels(vec![String::new()]).is_err());
        assert!(live_channels(vec!["x".repeat(129)]).is_err());
        assert!(live_channels((0..1025).map(|i| format!("c{i}")).collect()).is_err());
        assert!(live_channels((0..1024).map(|i| format!("c{i}")).collect()).is_ok());
    }
}
