//! One WebSocket to one relay, kept open for the life of a community session
//! (`live.ts` connection handling, `socket-requests.ts`).
//!
//! The socket authenticates with NIP-42 and holds every subscription and
//! publication until the relay accepts that authentication: a Buzz relay
//! always asks, so a connection that never authenticates is a failed one.
//! A refused or unsignable authentication is terminal until
//! [`RelaySocket::reconnect`]; a dropped connection is retried for as long
//! as it keeps dropping, after an equal-jitter delay of half to all of
//! `500 ms · 2ⁿ`, capped at [`SocketOptions::max_backoff`] (`live.ts`). A
//! full minute authenticated starts the backoff over, so each rollout of a
//! relay is recovered from promptly; a brief authentication does not, so a
//! flapping relay is not hammered.
//!
//! Publications are correlated with their `OK`, never replayed: an event
//! whose receipt was lost is reported as unconfirmed and the caller (the
//! outbox) decides what to do with the exact signed bytes.
//!
//! The port adds one thing the reference does not have: a heartbeat. It
//! pings every 25 s and treats 70 s of silence as a lost connection, since a
//! native socket, unlike a browser's, is not torn down when the network
//! changes under it.

use crate::event::{Verified, Verifier};
use crate::origin::RelayOrigin;
use buzz_identity::Identity;
use futures::{SinkExt, StreamExt};
use nostr::prelude::{Event, EventId};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// TLS for `wss://` trusts what the HTTP client trusts: the platform
/// verifier, as reqwest uses, so the system's roots and `SSL_CERT_FILE`.
/// The bundled web roots alone refuse a relay behind a private CA whose
/// HTTP reads succeed, and live updates then never connect. `None` (the
/// platform verifier could not start) falls back to the bundled roots.
fn tls() -> Option<tokio_tungstenite::Connector> {
    static CONFIG: std::sync::OnceLock<Option<Arc<rustls::ClientConfig>>> =
        std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            use rustls_platform_verifier::BuilderVerifierExt;
            let provider = rustls::crypto::CryptoProvider::get_default()
                .cloned()
                .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .and_then(|builder| builder.with_platform_verifier())
                .map(|builder| builder.with_no_client_auth());
            match config {
                Ok(config) => Some(Arc::new(config)),
                Err(error) => {
                    tracing::warn!(%error, "platform TLS verifier unavailable");
                    None
                }
            }
        })
        .clone()
        .map(tokio_tungstenite::Connector::Rustls)
}

/// A NIP-01 filter. Buzz relays accept extra fields (`search`,
/// `thread_window`, ...), so filters are plain JSON objects.
pub type Filter = serde_json::Map<String, Value>;

/// A filter from a `json!` object.
pub(crate) fn filter(value: Value) -> Filter {
    match value {
        Value::Object(map) => map,
        _ => unreachable!("filters are objects"),
    }
}

/// Inbound text frames larger than this are dropped unread (`live.ts`).
pub const MAX_FRAME: usize = 1024 * 1024;
/// Publications waiting for a receipt, sent or not.
pub const MAX_PUBLICATIONS: usize = 32;
/// Publications sent and not yet answered.
pub const MAX_UNACKED: usize = 3;
/// Largest `["EVENT", …]` frame a publication may use.
pub const MAX_PUBLICATION_FRAME: usize = 64 * 1024;
/// Largest `OK` message accepted as a receipt.
pub const MAX_RECEIPT_BYTES: usize = 16 * 1024;
/// The longest quota pause a relay hint may ask for.
pub const MAX_COOLDOWN_SECS: u64 = 86_400;

/// Connection progress. `Failed` waits for [`RelaySocket::reconnect`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketState {
    Connecting {
        attempt: u32,
    },
    Authenticating,
    Ready,
    /// The connection dropped; the next attempt starts after `retry_in`.
    Waiting {
        error: String,
        retry_in: Duration,
    },
    /// Authentication was refused or could not be signed, or reconnects
    /// were exhausted. Nothing more happens until `reconnect`.
    Failed {
        error: String,
    },
}

#[derive(Debug)]
pub enum SubscriptionEvent {
    Event(Verified),
    /// The relay sent an event that failed verification on this
    /// subscription.
    Invalid,
    /// Stored events have all been sent; what follows is live.
    Eose,
    /// The relay ended the subscription. It is not re-sent.
    Closed(String),
    /// The socket reconnected and re-sent this subscription. Events stored
    /// while disconnected arrive again, so consumers must dedupe by ID.
    Resubscribed,
    /// The connection dropped and this subscription was discarded (only
    /// when [`SocketOptions::resubscribe`] is off).
    Lost,
}

/// Why a publication has no positive receipt (`SocketRequestError`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    /// Proven never sent: refused locally, or failed before it left.
    #[error("{0}")]
    NotSent(String),
    /// The relay refused it with a reason that proves non-delivery
    /// (`invalid:`, `blocked:`, `restricted:`, `auth-required:`,
    /// `rate-limited:`).
    #[error("{0}")]
    Rejected(String),
    /// Sent, without a receipt that proves either outcome. The relay may
    /// have stored it.
    #[error("{message}")]
    Unconfirmed {
        message: String,
        /// The relay's own refusal text, when it gave one.
        refusal: Option<String>,
    },
}

impl PublishError {
    /// Whether the event may have reached the relay.
    pub fn sent(&self) -> bool {
        matches!(self, Self::Unconfirmed { .. })
    }
}

/// The outcome of a presence publication (`publishPresence`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresenceSend {
    Accepted,
    /// Refused, or no receipt in time.
    Unconfirmed,
    /// Not sent: no authenticated connection, or another presence
    /// publication is still waiting.
    NotSent,
    /// Not sent: the shared quota cooldown runs this much longer.
    Cooling(Duration),
}

/// The relay's quota cooldown for one connection owner (`createLiveAdmission`):
/// it gates REQ dispatch, publications and presence alike, and outlives
/// socket replacement.
#[derive(Clone, Default)]
pub struct Cooldown(Arc<Mutex<Option<Instant>>>);

impl std::fmt::Debug for Cooldown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cooldown({:?})", self.remaining())
    }
}

impl Cooldown {
    /// Pauses for `seconds` plus one: the relay reports whole seconds and
    /// retrying a moment early only earns another refusal.
    pub fn pause(&self, seconds: u64) {
        let until = Instant::now() + Duration::from_secs(seconds.min(MAX_COOLDOWN_SECS) + 1);
        let mut current = self.0.lock().expect("cooldown lock");
        *current = Some(current.map_or(until, |at| at.max(until)));
    }

    /// When the pause ends, if one is running.
    pub fn until(&self) -> Option<Instant> {
        let current = *self.0.lock().expect("cooldown lock");
        current.filter(|at| *at > Instant::now())
    }

    pub fn remaining(&self) -> Duration {
        self.until()
            .map_or(Duration::ZERO, |at| at - Instant::now())
    }
}

/// `rate-limited: quota exceeded; retry in Ns` → N, saturating: an absurd
/// hint is still a hint, and too long to honour.
pub fn quota_hint(reason: &str) -> Option<u64> {
    digits(
        reason
            .strip_prefix("rate-limited: quota exceeded; retry in ")?
            .strip_suffix('s')?,
    )
}

fn digits(text: &str) -> Option<u64> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().unwrap_or(u64::MAX))
}

#[derive(Clone, Debug)]
pub struct SocketOptions {
    /// From the start of an attempt until the relay accepts AUTH: connecting
    /// and authenticating share one deadline (`live.ts`, 10 s).
    pub auth_deadline: Duration,
    /// Test relays without NIP-42: treat the connection as authenticated
    /// when no challenge arrives within this long. Never set for Buzz
    /// relays, which always ask.
    pub open_after: Option<Duration>,
    /// From enqueue, including time waiting for authentication.
    pub publish_timeout: Duration,
    pub ping_interval: Duration,
    /// Reconnect when nothing at all arrives for this long.
    pub dead_after: Duration,
    /// The first reconnect delay; each later one doubles.
    pub min_backoff: Duration,
    /// The longest reconnect delay.
    pub max_backoff: Duration,
    /// How long a connection must stay authenticated before the next drop
    /// starts the backoff over.
    pub healthy_after: Duration,
    /// A sample in `0..1` that spreads each delay over half to all of it.
    pub jitter: fn() -> f64,
    /// Reconnects in a row without a healthy connection before the socket
    /// waits for [`RelaySocket::reconnect`]. `None` retries forever.
    pub max_reconnects: Option<u32>,
    /// Re-send open subscriptions after a reconnect. When off, they are
    /// reported [`SubscriptionEvent::Lost`] so the owner can rebuild them.
    pub resubscribe: bool,
    pub cooldown: Cooldown,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            auth_deadline: Duration::from_secs(10),
            open_after: None,
            publish_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(25),
            dead_after: Duration::from_secs(70),
            min_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            healthy_after: Duration::from_secs(60),
            jitter: random_unit,
            max_reconnects: None,
            resubscribe: true,
            cooldown: Cooldown::default(),
        }
    }
}

enum Command {
    Subscribe {
        id: u64,
        filters: Vec<Filter>,
        events: mpsc::UnboundedSender<SubscriptionEvent>,
    },
    Unsubscribe(u64),
    Publish {
        event: Box<Event>,
        reply: oneshot::Sender<Result<String, PublishError>>,
    },
    Presence {
        event: Box<Event>,
        reply: oneshot::Sender<PresenceSend>,
    },
    Reconnect,
}

pub struct RelaySocket {
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<SocketState>,
    next_id: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

/// Publishes on a [`RelaySocket`] without owning it. Once the socket is
/// dropped every publication fails as not sent.
#[derive(Clone)]
pub struct SocketPublisher {
    commands: mpsc::UnboundedSender<Command>,
}

impl SocketPublisher {
    /// Queues a signed event and waits for the relay's `OK`, returning its
    /// message.
    pub async fn publish(&self, event: Event) -> Result<String, PublishError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Publish {
                event: Box::new(event),
                reply,
            })
            .map_err(|_| PublishError::NotSent("Relay session disposed".into()))?;
        answer.await.unwrap_or_else(|_| {
            Err(PublishError::NotSent(
                "Socket connection interrupted".into(),
            ))
        })
    }

    /// Sends a presence event outside the publication queue: lossy, at most
    /// one waiting for its receipt, gated by the quota cooldown.
    pub async fn publish_presence(&self, event: Event) -> PresenceSend {
        let (reply, answer) = oneshot::channel();
        if self
            .commands
            .send(Command::Presence {
                event: Box::new(event),
                reply,
            })
            .is_err()
        {
            return PresenceSend::NotSent;
        }
        answer.await.unwrap_or(PresenceSend::NotSent)
    }
}

/// Live events for one REQ. Dropping it sends CLOSE.
pub struct Subscription {
    id: u64,
    events: mpsc::UnboundedReceiver<SubscriptionEvent>,
    commands: mpsc::UnboundedSender<Command>,
}

impl Subscription {
    pub async fn next(&mut self) -> Option<SubscriptionEvent> {
        self.events.recv().await
    }
}

impl futures::Stream for Subscription {
    type Item = SubscriptionEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<SubscriptionEvent>> {
        self.events.poll_recv(cx)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Unsubscribe(self.id));
    }
}

impl RelaySocket {
    pub fn connect(
        runtime: &tokio::runtime::Handle,
        origin: RelayOrigin,
        identity: Arc<Identity>,
        verifier: Arc<Verifier>,
        options: SocketOptions,
    ) -> Self {
        let (commands, queue) = mpsc::unbounded_channel();
        let (state_sender, state) = watch::channel(SocketState::Connecting { attempt: 1 });
        let task = runtime.spawn(
            Actor {
                viewer: identity.public_key(),
                origin,
                identity,
                verifier,
                options,
                state: state_sender,
                subscriptions: HashMap::new(),
                publications: Vec::new(),
                queue,
                subscribed_once: false,
                attempts: 0,
            }
            .run(),
        );
        Self {
            commands,
            state,
            next_id: Arc::new(AtomicU64::new(1)),
            task,
        }
    }

    pub fn state(&self) -> watch::Receiver<SocketState> {
        self.state.clone()
    }

    pub fn subscribe(&self, filters: Vec<Filter>) -> Subscription {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (events, receiver) = mpsc::unbounded_channel();
        let _ = self.commands.send(Command::Subscribe {
            id,
            filters,
            events,
        });
        Subscription {
            id,
            events: receiver,
            commands: self.commands.clone(),
        }
    }

    /// Queues a signed event and waits for the relay's `OK`.
    pub async fn publish(&self, event: Event) -> Result<String, PublishError> {
        self.publisher().publish(event).await
    }

    /// A handle that publishes on this socket from other tasks.
    pub fn publisher(&self) -> SocketPublisher {
        SocketPublisher {
            commands: self.commands.clone(),
        }
    }

    /// The user's retry: a fresh reconnect budget, and a new attempt at
    /// once unless the connection is already authenticated.
    pub fn reconnect(&self) {
        let _ = self.commands.send(Command::Reconnect);
    }
}

impl Drop for RelaySocket {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Open {
    filters: Vec<Filter>,
    events: mpsc::UnboundedSender<SubscriptionEvent>,
}

struct Publication {
    event: Box<Event>,
    sent: bool,
    deadline: Instant,
    reply: oneshot::Sender<Result<String, PublishError>>,
}

struct Actor {
    origin: RelayOrigin,
    identity: Arc<Identity>,
    viewer: nostr::prelude::PublicKey,
    verifier: Arc<Verifier>,
    options: SocketOptions,
    state: watch::Sender<SocketState>,
    subscriptions: HashMap<u64, Open>,
    /// In enqueue order. Kept across a backoff wait until their deadline;
    /// failed, never replayed, when a connection drops.
    publications: Vec<Publication>,
    queue: mpsc::UnboundedReceiver<Command>,
    /// An earlier connection already sent the subscriptions.
    subscribed_once: bool,
    /// Reconnects since the last healthy connection or the user's retry.
    attempts: u32,
}

type Stream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

enum End {
    /// The socket owner dropped the handle.
    Stopped,
    /// Retried after a backoff.
    Lost(String),
    /// Retried only on `reconnect`.
    Terminal(String),
    /// `reconnect` while not yet authenticated: start again at once.
    Restart,
}

enum Wait {
    Elapsed,
    Reconnect,
    Stopped,
}

impl Actor {
    async fn run(mut self) {
        loop {
            self.state.send_replace(SocketState::Connecting {
                attempt: self.attempts + 1,
            });
            let error = match self.session().await {
                End::Stopped => return,
                End::Restart => {
                    self.attempts = 0;
                    continue;
                }
                End::Terminal(error) => error,
                End::Lost(error) => {
                    if self
                        .options
                        .max_reconnects
                        .is_none_or(|max| self.attempts < max)
                    {
                        let ceiling = self
                            .options
                            .min_backoff
                            .saturating_mul(1 << self.attempts.min(16))
                            .min(self.options.max_backoff);
                        let sample = (self.options.jitter)().clamp(0.0, 1.0);
                        let retry_in = ceiling.mul_f64(0.5 + sample / 2.0);
                        self.attempts += 1;
                        tracing::debug!(origin = %self.origin, %error, ?retry_in, "relay socket lost");
                        self.state
                            .send_replace(SocketState::Waiting { error, retry_in });
                        match self.wait(Some(retry_in)).await {
                            Wait::Stopped => return,
                            Wait::Reconnect => self.attempts = 0,
                            Wait::Elapsed => {}
                        }
                        continue;
                    }
                    "Live reconnect attempts exhausted; retry available".to_string()
                }
            };
            tracing::warn!(origin = %self.origin, %error, "relay socket stopped");
            self.state.send_replace(SocketState::Failed { error });
            match self.wait(None).await {
                Wait::Stopped => return,
                Wait::Reconnect | Wait::Elapsed => self.attempts = 0,
            }
        }
    }

    /// Waits out a backoff (or, with no duration, for `reconnect`) while
    /// still accepting commands.
    async fn wait(&mut self, duration: Option<Duration>) -> Wait {
        let deadline = sleep_until(duration.map(|d| Instant::now() + d));
        tokio::pin!(deadline);
        loop {
            let publish_deadline = self.next_publication_deadline();
            tokio::select! {
                _ = &mut deadline => return Wait::Elapsed,
                _ = sleep_until(publish_deadline) => self.expire_publications(),
                command = self.queue.recv() => match command {
                    None => return Wait::Stopped,
                    Some(Command::Reconnect) => return Wait::Reconnect,
                    Some(command) => self.offline(command),
                },
            }
        }
    }

    /// Handles a command while there is no authenticated connection.
    fn offline(&mut self, command: Command) {
        match command {
            Command::Subscribe {
                id,
                filters,
                events,
            } => {
                self.subscriptions.insert(id, Open { filters, events });
            }
            Command::Unsubscribe(id) => {
                self.subscriptions.remove(&id);
            }
            Command::Publish { event, reply } => self.enqueue(event, reply),
            Command::Presence { reply, .. } => {
                let _ = reply.send(PresenceSend::NotSent);
            }
            Command::Reconnect => {}
        }
    }

    fn enqueue(&mut self, event: Box<Event>, reply: oneshot::Sender<Result<String, PublishError>>) {
        let refuse = |reply: oneshot::Sender<_>, message: &str| {
            let _ = reply.send(Err(PublishError::NotSent(message.into())));
        };
        if event.pubkey != self.viewer {
            return refuse(reply, "Publication signer does not match viewer");
        }
        if self.publications.iter().any(|p| p.event.id == event.id) {
            return refuse(reply, "Publication already in flight");
        }
        if self.publications.len() >= MAX_PUBLICATIONS {
            return refuse(reply, "Publication capacity reached");
        }
        if json!(["EVENT", event]).to_string().len() > MAX_PUBLICATION_FRAME {
            return refuse(reply, "Publication exceeds frame limit");
        }
        self.publications.push(Publication {
            event,
            sent: false,
            deadline: Instant::now() + self.options.publish_timeout,
            reply,
        });
    }

    fn next_publication_deadline(&self) -> Option<Instant> {
        self.publications.iter().map(|p| p.deadline).min()
    }

    fn expire_publications(&mut self) {
        let now = Instant::now();
        let (expired, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.publications)
            .into_iter()
            .partition(|p| p.deadline <= now);
        self.publications = kept;
        for publication in expired {
            finish(publication, "Publication timed out");
        }
    }

    /// Unsent publications whose turn has come: at most three awaiting a
    /// receipt, none during a quota cooldown.
    fn dispatchable(&mut self) -> Vec<String> {
        if self.options.cooldown.until().is_some() {
            return Vec::new();
        }
        let mut sent = self.publications.iter().filter(|p| p.sent).count();
        let mut frames = Vec::new();
        for publication in &mut self.publications {
            if sent >= MAX_UNACKED {
                break;
            }
            if !publication.sent {
                // Marked before sending: a failed send cannot prove that no
                // bytes left the process.
                publication.sent = true;
                sent += 1;
                frames.push(json!(["EVENT", publication.event]).to_string());
            }
        }
        frames
    }

    async fn session(&mut self) -> End {
        let deadline = Instant::now() + self.options.auth_deadline;
        let config = WebSocketConfig::default()
            .max_message_size(Some(16 * MAX_FRAME))
            .max_frame_size(Some(16 * MAX_FRAME));
        let connected = tokio::time::timeout_at(
            deadline,
            tokio_tungstenite::connect_async_tls_with_config(
                self.origin.websocket(),
                Some(config),
                false,
                tls(),
            ),
        )
        .await;
        let mut socket: Stream = match connected {
            Ok(Ok((socket, _))) => socket,
            Ok(Err(error)) => {
                tracing::debug!(origin = %self.origin, %error, "relay connection failed");
                return self.lost(None, End::Lost("Live connection interrupted".into()));
            }
            Err(_) => return self.lost(None, End::Lost("Live authentication timed out".into())),
        };
        tracing::info!(origin = %self.origin, "relay connected");

        let mut session = Session::default();
        let dead = tokio::time::sleep(self.options.dead_after);
        tokio::pin!(dead);
        let mut ping = tokio::time::interval(self.options.ping_interval);
        ping.reset();
        let open_after = sleep_until(self.options.open_after.map(|d| Instant::now() + d));
        tokio::pin!(open_after);
        let auth_deadline = tokio::time::sleep_until(deadline);
        tokio::pin!(auth_deadline);
        self.state.send_replace(SocketState::Authenticating);

        loop {
            // Wakes to dispatch once a cooldown ends, or to expire presence.
            let wake = [self.options.cooldown.until(), session.presence_deadline()]
                .into_iter()
                .flatten()
                .min();
            let publish_deadline = self.next_publication_deadline();
            let outgoing = tokio::select! {
                incoming = socket.next() => {
                    dead.as_mut().reset(Instant::now() + self.options.dead_after);
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            if text.len() > MAX_FRAME {
                                tracing::debug!(origin = %self.origin, bytes = text.len(), "dropping an oversized relay frame");
                                continue;
                            }
                            tracing::trace!(target: "relay-ws", origin = %self.origin, "← {}", text.as_str());
                            match self.receive(&mut session, text.as_str()) {
                                Ok(outgoing) => outgoing,
                                Err(end) => return self.lost(Some(&mut session), end),
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            return self.lost(Some(&mut session), End::Lost("Live connection closed".into()));
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(error)) => {
                            tracing::debug!(origin = %self.origin, %error, "relay connection error");
                            return self.lost(Some(&mut session), End::Lost("Live connection interrupted".into()));
                        }
                    }
                }
                _ = &mut open_after, if !session.authenticated && !session.authenticating => {
                    self.authenticated(&mut session)
                }
                command = self.queue.recv() => {
                    let Some(command) = command else {
                        let _ = socket.close(None).await;
                        return End::Stopped;
                    };
                    match command {
                        Command::Reconnect if session.authenticated => {
                            self.attempts = 0;
                            Vec::new()
                        }
                        Command::Reconnect => return self.lost(Some(&mut session), End::Restart),
                        command => self.command(&mut session, command),
                    }
                }
                _ = ping.tick() => {
                    if let Err(error) = socket.send(Message::Ping(Vec::new().into())).await {
                        tracing::debug!(origin = %self.origin, %error, "relay ping failed");
                        return self.lost(Some(&mut session), End::Lost("Live connection interrupted".into()));
                    }
                    continue;
                }
                _ = &mut dead => {
                    tracing::debug!(origin = %self.origin, "relay stopped responding");
                    return self.lost(Some(&mut session), End::Lost("Live connection interrupted".into()));
                }
                _ = &mut auth_deadline, if !session.authenticated => {
                    return self.lost(Some(&mut session), End::Lost("Live authentication timed out".into()));
                }
                _ = sleep_until(publish_deadline) => {
                    self.expire_publications();
                    Vec::new()
                }
                _ = sleep_until(wake) => {
                    session.expire_presence();
                    Vec::new()
                }
            };
            let mut frames = outgoing;
            if session.authenticated {
                frames.extend(self.dispatchable());
            }
            for message in frames {
                tracing::trace!(target: "relay-ws", origin = %self.origin, "→ {message}");
                if let Err(error) = socket.send(Message::Text(message.into())).await {
                    tracing::debug!(origin = %self.origin, %error, "relay send failed");
                    return self.lost(
                        Some(&mut session),
                        End::Lost("Live connection interrupted".into()),
                    );
                }
            }
        }
    }

    /// Fails everything waiting on this connection (`clearSocket`) and keeps
    /// subscriptions for the next one.
    fn lost(&mut self, session: Option<&mut Session>, end: End) -> End {
        if let Some(session) = session {
            if session
                .authenticated_at
                .is_some_and(|at| at.elapsed() >= self.options.healthy_after)
            {
                self.attempts = 0;
            }
            if let Some((_, reply, _)) = session.presence.take() {
                let _ = reply.send(PresenceSend::Unconfirmed);
            }
        }
        for publication in std::mem::take(&mut self.publications) {
            finish(publication, "Socket connection interrupted");
        }
        if !self.options.resubscribe {
            for (_, open) in self.subscriptions.drain() {
                let _ = open.events.send(SubscriptionEvent::Lost);
            }
        }
        end
    }

    fn authenticated(&mut self, session: &mut Session) -> Vec<String> {
        session.authenticated = true;
        session.authenticated_at = Some(Instant::now());
        self.state.send_replace(SocketState::Ready);
        self.resubscribe_all()
    }

    fn command(&mut self, session: &mut Session, command: Command) -> Vec<String> {
        match command {
            Command::Subscribe {
                id,
                filters,
                events,
            } => {
                let message = session.authenticated.then(|| req(id, &filters));
                self.subscriptions.insert(id, Open { filters, events });
                message.into_iter().collect()
            }
            Command::Unsubscribe(id) => {
                let was_open = self.subscriptions.remove(&id).is_some();
                if was_open && session.authenticated {
                    vec![json!(["CLOSE", sub_id(id)]).to_string()]
                } else {
                    Vec::new()
                }
            }
            Command::Publish { event, reply } => {
                self.enqueue(event, reply);
                Vec::new()
            }
            Command::Presence { event, reply } => {
                if !session.authenticated || session.presence.is_some() {
                    let _ = reply.send(PresenceSend::NotSent);
                    return Vec::new();
                }
                if let Some(until) = self.options.cooldown.until() {
                    let _ = reply.send(PresenceSend::Cooling(until - Instant::now()));
                    return Vec::new();
                }
                let message = json!(["EVENT", event]).to_string();
                session.presence = Some((event.id, reply, Instant::now() + PRESENCE_RECEIPT));
                vec![message]
            }
            Command::Reconnect => Vec::new(),
        }
    }

    fn resubscribe_all(&mut self) -> Vec<String> {
        let resubscribed = self.subscribed_once;
        self.subscribed_once = true;
        self.subscriptions
            .retain(|_, open| !open.events.is_closed());
        self.subscriptions
            .iter()
            .map(|(id, open)| {
                if resubscribed {
                    let _ = open.events.send(SubscriptionEvent::Resubscribed);
                }
                req(*id, &open.filters)
            })
            .collect()
    }

    fn receive(&mut self, session: &mut Session, text: &str) -> Result<Vec<String>, End> {
        let Ok(Value::Array(message)) = serde_json::from_str::<Value>(text) else {
            tracing::debug!(origin = %self.origin, "ignoring malformed relay message");
            return Ok(Vec::new());
        };
        let kind = message.first().and_then(Value::as_str).unwrap_or("");
        match kind {
            "AUTH" => {
                // One challenge per connection.
                let Some(challenge) = message.get(1).and_then(Value::as_str) else {
                    return Ok(Vec::new());
                };
                if session.authenticating {
                    return Ok(Vec::new());
                }
                session.authenticating = true;
                match self.identity.relay_auth(self.origin.websocket(), challenge) {
                    Ok(event) if event.pubkey != self.viewer => {
                        Err(End::Terminal("Live signer does not match viewer".into()))
                    }
                    Ok(event) => {
                        session.auth_event = Some(event.id);
                        Ok(vec![json!(["AUTH", event]).to_string()])
                    }
                    Err(error) => {
                        tracing::error!(%error, "could not sign relay authentication");
                        Err(End::Terminal("Live authentication signing failed".into()))
                    }
                }
            }
            "OK" => {
                let id = message
                    .get(1)
                    .and_then(Value::as_str)
                    .and_then(|id| EventId::from_hex(id).ok());
                let Some(id) = id else {
                    return Ok(Vec::new());
                };
                if session.auth_event == Some(id) && !session.authenticated {
                    if message.get(2) != Some(&Value::Bool(true)) {
                        return Err(End::Terminal("Relay rejected live authentication".into()));
                    }
                    return Ok(self.authenticated(session));
                }
                if session.presence.as_ref().is_some_and(|(p, _, _)| *p == id) {
                    let (_, reply, _) = session.presence.take().expect("presence receipt");
                    let accepted = message.get(2) == Some(&Value::Bool(true));
                    if let Some(reason) = message.get(3).and_then(Value::as_str)
                        && !accepted
                        && reason.starts_with("rate-limited:")
                    {
                        // A presence refusal with no hint stops everything
                        // for a day rather than draining a busy relay.
                        self.options
                            .cooldown
                            .pause(quota_hint(reason).unwrap_or(MAX_COOLDOWN_SECS));
                    }
                    let _ = reply.send(if accepted {
                        PresenceSend::Accepted
                    } else {
                        PresenceSend::Unconfirmed
                    });
                    return Ok(Vec::new());
                }
                if !session.authenticated {
                    return Ok(Vec::new());
                }
                let Some(index) = self
                    .publications
                    .iter()
                    .position(|p| p.sent && p.event.id == id)
                else {
                    return Ok(Vec::new());
                };
                let publication = self.publications.remove(index);
                let result = receipt(&message, publication.event.kind.as_u16());
                if let Err(PublishError::Rejected(reason)) = &result
                    && reason.starts_with("rate-limited:")
                {
                    let seconds = reason
                        .rsplit_once("retry in ")
                        .and_then(|(_, rest)| digits(rest.strip_suffix('s')?));
                    self.options.cooldown.pause(seconds.unwrap_or(5));
                }
                let _ = publication.reply.send(result);
                Ok(Vec::new())
            }
            _ if !session.authenticated => Ok(Vec::new()),
            "EVENT" => {
                let (Some(id), Some(event)) =
                    (message.get(1).and_then(parse_sub_id), message.get(2))
                else {
                    return Ok(Vec::new());
                };
                let Some(open) = self.subscriptions.get(&id) else {
                    return Ok(Vec::new());
                };
                match self.verifier.verify_json(event) {
                    Ok(event) => {
                        let _ = open.events.send(SubscriptionEvent::Event(event));
                    }
                    Err(_) => {
                        tracing::warn!(origin = %self.origin, "relay sent an invalid live event");
                        let _ = open.events.send(SubscriptionEvent::Invalid);
                    }
                }
                Ok(Vec::new())
            }
            "EOSE" => {
                if let Some(open) = message
                    .get(1)
                    .and_then(parse_sub_id)
                    .and_then(|id| self.subscriptions.get(&id))
                {
                    let _ = open.events.send(SubscriptionEvent::Eose);
                }
                Ok(Vec::new())
            }
            "CLOSED" => {
                let Some(id) = message.get(1).and_then(parse_sub_id) else {
                    return Ok(Vec::new());
                };
                let reason = message
                    .get(2)
                    .and_then(Value::as_str)
                    .map_or("Relay closed live subscription".into(), |reason| {
                        reason.chars().take(512).collect::<String>()
                    });
                if let Some(open) = self.subscriptions.remove(&id) {
                    let _ = open.events.send(SubscriptionEvent::Closed(reason));
                }
                Ok(Vec::new())
            }
            "NOTICE" => {
                let notice = message.get(1).and_then(Value::as_str).unwrap_or("");
                tracing::info!(origin = %self.origin, %notice, "relay notice");
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }
}

/// A presence publication counts as unconfirmed after this.
const PRESENCE_RECEIPT: Duration = Duration::from_secs(10);

/// Maps an `OK` for a sent publication (`socket-requests.ts`). Only
/// documented admission and validation refusals prove non-delivery; any
/// other negative answer may follow a side effect.
fn receipt(message: &[Value], kind: u16) -> Result<String, PublishError> {
    let unconfirmed = |message: &str, refusal: Option<String>| PublishError::Unconfirmed {
        message: message.into(),
        refusal,
    };
    let (Some(Value::Bool(accepted)), Some(Value::String(reason))) =
        (message.get(2), message.get(3))
    else {
        return Err(unconfirmed("Invalid publication receipt", None));
    };
    if message.len() != 4 || reason.len() > MAX_RECEIPT_BYTES {
        return Err(unconfirmed("Invalid publication receipt", None));
    }
    if *accepted {
        return Ok(reason.clone());
    }
    let prefix = |prefixes: &[&str]| {
        prefixes.iter().any(|p| {
            reason
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with(':'))
        })
    };
    let rejected = prefix(&[
        "invalid",
        "blocked",
        "restricted",
        "auth-required",
        "rate-limited",
    ]) || (matches!(kind, 30620 | 46020) && prefix(&["conflict", "forbidden"]))
        || (matches!(kind, 40100 | 45010) && prefix(&["conflict"]));
    if rejected {
        Err(PublishError::Rejected(reason.clone()))
    } else {
        Err(unconfirmed(
            "Relay publication not confirmed",
            Some(publication_refusal(reason)),
        ))
    }
}

/// A relay refusal safe to show (`publicationRefusal`): known reasons and
/// exact quota hints verbatim, anything else only by its category.
pub fn publication_refusal(reason: &str) -> String {
    const KNOWN: &[&str] = &[
        "rate-limited: shared admission unavailable",
        "invalid: event timestamp too far from server time",
        "invalid: event pubkey does not match authenticated identity",
        "restricted: channel-scoped tokens cannot publish global events",
        "restricted: community writes are fenced",
        "restricted: insufficient scope (need users:write)",
        "restricted: insufficient scope (need admin:channels)",
        "error: internal server error",
        "invalid: policy:owner_only — agent has no owner set",
        "invalid: policy:owner_only — only the agent owner can add this agent",
        "invalid: policy:nobody — this agent has disabled external channel additions",
        "invalid: actor not authorized",
        "invalid: channel not found",
        "invalid: channel is archived",
        "restricted: not a channel member",
    ];
    let quota = reason
        .strip_prefix("rate-limited: quota exceeded; retry in ")
        .and_then(|rest| rest.strip_suffix('s'))
        .is_some_and(|n| (1..=5).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()));
    if KNOWN.contains(&reason) || quota {
        return reason.to_string();
    }
    const CATEGORIES: &[&str] = &[
        "invalid",
        "blocked",
        "restricted",
        "auth-required",
        "rate-limited",
        "error",
        "conflict",
        "forbidden",
    ];
    match CATEGORIES.iter().find(|category| {
        reason
            .strip_prefix(**category)
            .is_some_and(|rest| rest.starts_with(':'))
    }) {
        Some(category) => format!("{category}: unrecognized reason"),
        None => "unrecognized reason".into(),
    }
}

/// Fails a publication with `message`, as sent or not as it was.
fn finish(publication: Publication, message: &str) {
    let error = if publication.sent {
        PublishError::Unconfirmed {
            message: message.into(),
            refusal: None,
        }
    } else {
        PublishError::NotSent(message.into())
    };
    let _ = publication.reply.send(Err(error));
}

type PresenceReceipt = (EventId, oneshot::Sender<PresenceSend>, Instant);

#[derive(Default)]
struct Session {
    /// The relay's challenge was answered on this connection.
    authenticating: bool,
    /// The AUTH event sent for this connection.
    auth_event: Option<EventId>,
    /// The relay accepted our AUTH: subscriptions and publications go out.
    authenticated: bool,
    authenticated_at: Option<Instant>,
    presence: Option<PresenceReceipt>,
}

/// A uniform sample in `0..1` (`Math.random`), from the standard library's
/// randomly seeded hasher, which is plenty for spreading reconnects.
fn random_unit() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let bits = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

impl Session {
    fn presence_deadline(&self) -> Option<Instant> {
        self.presence.as_ref().map(|(_, _, deadline)| *deadline)
    }

    fn expire_presence(&mut self) {
        if self
            .presence_deadline()
            .is_some_and(|deadline| deadline <= Instant::now())
            && let Some((_, reply, _)) = self.presence.take()
        {
            let _ = reply.send(PresenceSend::Unconfirmed);
        }
    }
}

fn req(id: u64, filters: &[Filter]) -> String {
    let mut message = vec![json!("REQ"), json!(sub_id(id))];
    message.extend(filters.iter().cloned().map(Value::Object));
    Value::Array(message).to_string()
}

fn sub_id(id: u64) -> String {
    format!("s{id}")
}

fn parse_sub_id(value: &Value) -> Option<u64> {
    value.as_str()?.strip_prefix('s')?.parse().ok()
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
    fn quota_hints_parse_whole_seconds() {
        assert_eq!(
            quota_hint("rate-limited: quota exceeded; retry in 7s"),
            Some(7)
        );
        assert_eq!(
            quota_hint("rate-limited: shared admission unavailable"),
            None
        );
        assert_eq!(
            quota_hint("rate-limited: quota exceeded; retry in xs"),
            None
        );
    }

    #[test]
    fn only_documented_refusals_prove_non_delivery() {
        let ok = |accepted: bool, reason: &str| {
            vec![json!("OK"), json!("id"), json!(accepted), json!(reason)]
        };
        assert_eq!(receipt(&ok(true, "stored"), 9), Ok("stored".into()));
        for reason in [
            "invalid: bad",
            "blocked: no",
            "restricted: x",
            "auth-required: y",
            "rate-limited: z",
        ] {
            assert_eq!(
                receipt(&ok(false, reason), 9),
                Err(PublishError::Rejected(reason.into()))
            );
        }
        for (reason, shown) in [
            (
                "error: internal server error",
                "error: internal server error",
            ),
            ("error: internal", "error: unrecognized reason"),
            ("duplicate: have it", "unrecognized reason"),
            ("conflict: stale", "conflict: unrecognized reason"),
        ] {
            assert!(matches!(
                receipt(&ok(false, reason), 9),
                Err(PublishError::Unconfirmed { refusal: Some(r), .. }) if r == shown
            ));
        }
        // Canvas and workflow CAS refusals precede the mutation.
        assert!(matches!(
            receipt(&ok(false, "conflict: stale"), 40100),
            Err(PublishError::Rejected(_))
        ));
        assert!(matches!(
            receipt(&ok(false, "forbidden: no"), 30620),
            Err(PublishError::Rejected(_))
        ));
        let malformed = vec![json!("OK"), json!("id"), json!(true)];
        assert!(matches!(
            receipt(&malformed, 9),
            Err(PublishError::Unconfirmed { refusal: None, .. })
        ));
        let huge = ok(true, &"x".repeat(MAX_RECEIPT_BYTES + 1));
        assert!(matches!(
            receipt(&huge, 9),
            Err(PublishError::Unconfirmed { .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn cooldown_pauses_whole_seconds_plus_one_and_only_extends() {
        let cooldown = Cooldown::default();
        assert_eq!(cooldown.until(), None);
        cooldown.pause(3);
        assert_eq!(cooldown.remaining(), Duration::from_secs(4));
        cooldown.pause(1);
        assert_eq!(
            cooldown.remaining(),
            Duration::from_secs(4),
            "never shortened"
        );
        cooldown.pause(10 * MAX_COOLDOWN_SECS);
        assert_eq!(
            cooldown.remaining(),
            Duration::from_secs(MAX_COOLDOWN_SECS + 1)
        );
        tokio::time::advance(Duration::from_secs(MAX_COOLDOWN_SECS + 2)).await;
        assert_eq!(cooldown.until(), None);
    }
}
