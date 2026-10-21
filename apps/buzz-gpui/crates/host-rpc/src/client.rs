use crate::frame::{self, Frame, Hello, HostInfo, PROTOCOL_VERSION};
use crate::link::{self, Pending};
use crate::transport::Connector;
use crate::{Protocol, RpcError};
use futures::StreamExt;
use futures::future::BoxFuture;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify, broadcast, mpsc, watch};

/// Answers requests the host sends to the GUI.
pub trait ClientHandler<P: Protocol>: Send + Sync + 'static {
    fn handle(
        &self,
        request: P::HostRequest,
    ) -> BoxFuture<'static, Result<P::HostResponse, RpcError>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting {
        attempt: u32,
    },
    Connected {
        host: HostInfo,
    },
    /// Waiting before the next attempt.
    Disconnected {
        error: String,
        retry_in: Duration,
    },
    /// The host refused this client (bad token, incompatible version). No
    /// further attempts are made until [`HostClient::reconnect`] is called.
    Rejected {
        reason: String,
    },
}

#[derive(Clone, Debug)]
pub enum ClientEvent<E> {
    /// A new session started. Anything cached from a previous session may be
    /// stale and should be fetched again.
    Connected(HostInfo),
    Event {
        seq: u64,
        body: E,
    },
    /// Events were dropped; fetch fresh state.
    Resync,
    Disconnected,
}

#[derive(Clone)]
pub struct HostClientOptions {
    pub client_name: String,
    pub token: Option<String>,
    pub request_timeout: Duration,
    pub ping_interval: Duration,
    /// The connection is considered dead after this long without any frame.
    pub dead_after: Duration,
    pub min_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for HostClientOptions {
    fn default() -> Self {
        Self {
            client_name: "buzz-gpui".into(),
            token: None,
            request_timeout: Duration::from_secs(60),
            ping_interval: Duration::from_secs(5),
            dead_after: Duration::from_secs(15),
            min_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(10),
        }
    }
}

struct Link<P: Protocol> {
    outgoing: mpsc::Sender<Frame<P>>,
    pending: Pending<P::ClientResponse>,
}

struct Shared<P: Protocol> {
    link: Mutex<Option<Link<P>>>,
    state: watch::Sender<ConnectionState>,
    events: broadcast::Sender<ClientEvent<P::Event>>,
    wake: Notify,
    next_id: AtomicU64,
    options: HostClientOptions,
}

/// A connection to one host that reconnects on its own until dropped.
pub struct HostClient<P: Protocol> {
    shared: Arc<Shared<P>>,
    task: tokio::task::JoinHandle<()>,
}

impl<P: Protocol> HostClient<P> {
    pub fn spawn(
        runtime: &tokio::runtime::Handle,
        connector: Arc<dyn Connector>,
        handler: Arc<dyn ClientHandler<P>>,
        options: HostClientOptions,
    ) -> Self {
        let (events, _) = broadcast::channel(1024);
        let shared = Arc::new(Shared {
            link: Mutex::new(None),
            state: watch::Sender::new(ConnectionState::Connecting { attempt: 1 }),
            events,
            wake: Notify::new(),
            next_id: AtomicU64::new(1),
            options,
        });
        let task = runtime.spawn(run(shared.clone(), connector, handler));
        Self { shared, task }
    }

    pub fn state(&self) -> watch::Receiver<ConnectionState> {
        self.shared.state.subscribe()
    }

    pub fn events(&self) -> broadcast::Receiver<ClientEvent<P::Event>> {
        self.shared.events.subscribe()
    }

    /// Skips the current backoff, or retries after a rejection.
    pub fn reconnect(&self) {
        self.shared.wake.notify_one();
    }

    pub async fn request(&self, body: P::ClientRequest) -> Result<P::ClientResponse, RpcError> {
        self.request_within(body, self.shared.options.request_timeout)
            .await
    }

    /// As [`Self::request`], waiting up to `timeout` for the answer: for
    /// requests that wait on a person or an installer.
    pub async fn request_within(
        &self,
        body: P::ClientRequest,
        timeout: std::time::Duration,
    ) -> Result<P::ClientResponse, RpcError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        // Not held while sending: a full queue would block every other
        // request, and the session ending, behind this one.
        let (outgoing, pending) = {
            let link = self.shared.link.lock().await;
            let link = link.as_ref().ok_or(RpcError::NotConnected)?;
            (link.outgoing.clone(), link.pending.clone())
        };
        let frame = Frame::ClientRequest { id, body };
        link::request(&outgoing, &pending, id, frame, timeout).await
    }
}

impl<P: Protocol> Drop for HostClient<P> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum SessionEnd {
    Lost(String),
    Rejected(String),
}

async fn run<P: Protocol>(
    shared: Arc<Shared<P>>,
    connector: Arc<dyn Connector>,
    handler: Arc<dyn ClientHandler<P>>,
) {
    let mut attempt = 1;
    loop {
        shared
            .state
            .send_replace(ConnectionState::Connecting { attempt });
        let mut welcomed = None;
        let end = session(&shared, connector.as_ref(), handler.clone(), &mut welcomed).await;
        // Only a session that stayed up starts the backoff over, so a host
        // that welcomes and then drops is not retried at full speed.
        if welcomed
            .is_some_and(|at: tokio::time::Instant| at.elapsed() >= shared.options.dead_after)
        {
            attempt = 1;
        }
        if let Some(link) = shared.link.lock().await.take() {
            for (_, sender) in link.pending.lock().await.drain() {
                let _ = sender.send(Err(RpcError::Disconnected));
            }
            let _ = shared.events.send(ClientEvent::Disconnected);
        }
        match end {
            SessionEnd::Rejected(reason) => {
                tracing::warn!(target = %connector.describe(), %reason, "host rejected client");
                shared
                    .state
                    .send_replace(ConnectionState::Rejected { reason });
                shared.wake.notified().await;
                attempt = 1;
            }
            SessionEnd::Lost(error) => {
                let retry_in = backoff(&shared.options, attempt);
                tracing::debug!(target = %connector.describe(), %error, ?retry_in, "host connection lost");
                shared
                    .state
                    .send_replace(ConnectionState::Disconnected { error, retry_in });
                tokio::select! {
                    _ = tokio::time::sleep(retry_in) => {}
                    _ = shared.wake.notified() => {}
                }
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

fn backoff(options: &HostClientOptions, attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(16);
    options
        .min_backoff
        .saturating_mul(1 << exponent)
        .min(options.max_backoff)
}

async fn session<P: Protocol>(
    shared: &Arc<Shared<P>>,
    connector: &dyn Connector,
    handler: Arc<dyn ClientHandler<P>>,
    welcomed: &mut Option<tokio::time::Instant>,
) -> SessionEnd {
    let options = &shared.options;
    let connection = match tokio::time::timeout(options.dead_after, connector.connect()).await {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => return SessionEnd::Lost(error.to_string()),
        Err(_) => return SessionEnd::Lost("connect timed out".into()),
    };
    let mut wire = frame::wire(connection);
    let hello = Frame::<P>::Hello(Hello {
        protocol: PROTOCOL_VERSION,
        token: options.token.clone(),
        client: options.client_name.clone(),
    });
    if let Err(error) = frame::send(&mut wire, &hello).await {
        return SessionEnd::Lost(error.to_string());
    }
    let welcome =
        match tokio::time::timeout(options.dead_after, frame::receive::<P>(&mut wire)).await {
            Ok(Ok(Frame::Welcome(welcome))) => welcome,
            Ok(Ok(Frame::Rejected { reason })) => return SessionEnd::Rejected(reason),
            Ok(Ok(_)) => return SessionEnd::Lost("unexpected handshake frame".into()),
            Ok(Err(error)) => return SessionEnd::Lost(error.to_string()),
            Err(_) => return SessionEnd::Lost("handshake timed out".into()),
        };
    if welcome.protocol != PROTOCOL_VERSION {
        return SessionEnd::Rejected(format!(
            "host speaks protocol {}, this app speaks {PROTOCOL_VERSION}",
            welcome.protocol
        ));
    }

    frame::handshaken(&mut wire);
    let (sink, mut stream) = wire.split();
    let (outgoing, queue) = mpsc::channel::<Frame<P>>(256);
    let pending: Pending<P::ClientResponse> = Arc::default();
    *shared.link.lock().await = Some(Link {
        outgoing: outgoing.clone(),
        pending: pending.clone(),
    });
    *welcomed = Some(tokio::time::Instant::now());
    shared.state.send_replace(ConnectionState::Connected {
        host: welcome.host.clone(),
    });
    let _ = shared.events.send(ClientEvent::Connected(welcome.host));

    let mut writer = AbortOnDrop(tokio::spawn(link::write(sink, queue, pending.clone())));
    let mut ping = tokio::time::interval(options.ping_interval);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut nonce = 0u64;
    // Reset only by frames from the host, so our own pings cannot keep a
    // silent connection alive.
    let dead = tokio::time::sleep(options.dead_after);
    tokio::pin!(dead);

    loop {
        tokio::select! {
            incoming = stream.next() => {
                let bytes = match incoming {
                    Some(Ok(bytes)) => bytes,
                    Some(Err(error)) => return SessionEnd::Lost(error.to_string()),
                    None => return SessionEnd::Lost("host closed the connection".into()),
                };
                dead.as_mut().reset(tokio::time::Instant::now() + options.dead_after);
                let unsupported = |id| Frame::HostResponse {
                    id,
                    body: Err(frame::unsupported()),
                };
                let frame = match link::read(&bytes, &outgoing, unsupported).await {
                    Ok(Some(frame)) => frame,
                    Ok(None) => continue,
                    Err(error) => return SessionEnd::Lost(error.to_string()),
                };
                match frame {
                    Frame::ClientResponse { id, body } => {
                        if let Some(sender) = pending.lock().await.remove(&id) {
                            let _ = sender.send(body);
                        }
                    }
                    Frame::Event { seq, body } => {
                        let _ = shared.events.send(ClientEvent::Event { seq, body });
                    }
                    Frame::Resync => {
                        let _ = shared.events.send(ClientEvent::Resync);
                    }
                    Frame::HostRequest { id, body } => {
                        let response = handler.handle(body);
                        let outgoing = outgoing.clone();
                        tokio::spawn(async move {
                            let body = response.await;
                            let _ = outgoing.send(Frame::HostResponse { id, body }).await;
                        });
                    }
                    Frame::Pong { .. } => {}
                    _ => return SessionEnd::Lost("unexpected frame from host".into()),
                }
            }
            _ = &mut dead => {
                return SessionEnd::Lost("host stopped responding".into());
            }
            _ = ping.tick() => {
                nonce += 1;
                if outgoing.send(Frame::Ping { nonce }).await.is_err() {
                    return SessionEnd::Lost("connection writer stopped".into());
                }
            }
            result = &mut writer.0 => {
                let error = match result {
                    Ok(Ok(())) => "connection writer stopped".to_string(),
                    Ok(Err(error)) => error.to_string(),
                    Err(error) => error.to_string(),
                };
                return SessionEnd::Lost(error);
            }
        }
    }
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
