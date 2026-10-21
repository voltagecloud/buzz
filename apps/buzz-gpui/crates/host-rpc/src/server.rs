use crate::frame::{self, Frame, Hello, HostInfo, PROTOCOL_VERSION, Welcome, Wire};
use crate::link;
use crate::transport::{BoxConnection, Listener};
use crate::{Protocol, RpcError};
use futures::StreamExt;
use futures::future::BoxFuture;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::{broadcast, mpsc};

/// How long to wait after a failed accept, so a listener out of file
/// descriptors does not spin.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// Events the host broadcasts, tagged with a sequence number that increases
/// for the lifetime of the host process.
pub type EventSource<P> = broadcast::Receiver<(u64, <P as Protocol>::Event)>;

pub trait HostHandler<P: Protocol>: Send + Sync + 'static {
    fn info(&self) -> HostInfo;
    fn events(&self) -> EventSource<P>;
    fn handle(
        &self,
        peer: Peer<P>,
        request: P::ClientRequest,
    ) -> BoxFuture<'static, Result<P::ClientResponse, RpcError>>;
}

#[derive(Clone)]
pub struct ServerOptions {
    /// Required from every client when set. Only the in-memory transport
    /// inside the GUI process should run without one.
    pub token: Option<String>,
    pub handshake_timeout: Duration,
    /// A client that sends nothing (not even a ping) for this long is dropped.
    pub idle_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            token: None,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(120),
        }
    }
}

/// A connected GUI, used by the host to send it requests.
pub struct Peer<P: Protocol> {
    address: Arc<str>,
    outgoing: mpsc::Sender<Frame<P>>,
    pending: link::Pending<P::HostResponse>,
    next_id: Arc<AtomicU64>,
    timeout: Duration,
}

impl<P: Protocol> Clone for Peer<P> {
    fn clone(&self) -> Self {
        Self {
            address: self.address.clone(),
            outgoing: self.outgoing.clone(),
            pending: self.pending.clone(),
            next_id: self.next_id.clone(),
            timeout: self.timeout,
        }
    }
}

impl<P: Protocol> Peer<P> {
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Resolves once this connection has closed, so work the GUI started
    /// over it can stop with it.
    pub async fn closed(&self) {
        self.outgoing.closed().await
    }

    /// Whether this connection has closed.
    pub fn is_closed(&self) -> bool {
        self.outgoing.is_closed()
    }

    /// Whether `other` came from the same connection as this peer.
    pub fn same_connection(&self, other: &Self) -> bool {
        self.outgoing.same_channel(&other.outgoing)
    }

    pub async fn request(&self, body: P::HostRequest) -> Result<P::HostResponse, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = Frame::HostRequest { id, body };
        link::request(&self.outgoing, &self.pending, id, frame, self.timeout).await
    }
}

/// Accepts connections until the listener closes. Each connection runs on
/// its own task, so one slow or broken client never blocks another. Dropping
/// the returned future closes every connection it accepted.
pub async fn serve<P: Protocol, L: Listener>(
    mut listener: L,
    handler: Arc<dyn HostHandler<P>>,
    options: ServerOptions,
) -> std::io::Result<()> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (connection, address) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                        return Err(error);
                    }
                    // One connection failing, or no file descriptors
                    // left for now, must not stop the host.
                    Err(error) => {
                        tracing::warn!(%error, "could not accept a host connection");
                        tokio::time::sleep(ACCEPT_PAUSE).await;
                        continue;
                    }
                };
                let handler = handler.clone();
                let options = options.clone();
                connections.spawn(async move {
                    if let Err(error) = run_connection(connection, &address, handler, options).await {
                        tracing::debug!(%address, %error, "host connection closed");
                    }
                });
            }
            Some(_) = connections.join_next() => {}
        }
    }
}

async fn run_connection<P: Protocol>(
    connection: BoxConnection,
    address: &str,
    handler: Arc<dyn HostHandler<P>>,
    options: ServerOptions,
) -> std::io::Result<()> {
    let mut wire = frame::wire(connection);
    let hello =
        match tokio::time::timeout(options.handshake_timeout, frame::receive::<P>(&mut wire)).await
        {
            Ok(Ok(Frame::Hello(hello))) => hello,
            Ok(Ok(_)) => return reject::<P>(&mut wire, "expected hello").await,
            Ok(Err(error)) => return Err(error),
            Err(_) => return reject::<P>(&mut wire, "handshake timed out").await,
        };
    if let Some(reason) = check_hello(&hello, options.token.as_deref()) {
        tracing::warn!(%address, client = shown(&hello.client), reason, "rejected host client");
        return reject::<P>(&mut wire, reason).await;
    }
    // Subscribe before welcoming, so no event between the client's snapshot
    // request and its subscription can be missed.
    let mut events = handler.events();
    frame::send::<P>(
        &mut wire,
        &Frame::Welcome(Welcome {
            protocol: PROTOCOL_VERSION,
            host: handler.info(),
        }),
    )
    .await?;
    frame::handshaken(&mut wire);
    tracing::info!(%address, client = shown(&hello.client), "host client connected");

    let (sink, mut stream) = wire.split();
    let (outgoing, queue) = mpsc::channel::<Frame<P>>(256);
    let peer = Peer {
        address: address.into(),
        outgoing: outgoing.clone(),
        pending: Arc::default(),
        next_id: Arc::new(AtomicU64::new(1)),
        timeout: options.request_timeout,
    };

    // Everything spawned for this connection stops with it.
    let mut writer = tokio::task::JoinSet::new();
    writer.spawn(link::write(sink, queue, peer.pending.clone()));
    let mut tasks = tokio::task::JoinSet::new();
    let idle = tokio::time::sleep(options.idle_timeout);
    tokio::pin!(idle);

    loop {
        tokio::select! {
            incoming = stream.next() => {
                let bytes = match incoming {
                    Some(Ok(bytes)) => bytes,
                    Some(Err(error)) => return Err(error),
                    None => return Ok(()),
                };
                idle.as_mut().reset(tokio::time::Instant::now() + options.idle_timeout);
                let Some(frame) = link::read(&bytes, &outgoing, |id| Frame::ClientResponse {
                    id,
                    body: Err(frame::unsupported()),
                })
                .await?
                else {
                    continue;
                };
                match frame {
                    Frame::ClientRequest { id, body } => {
                        let response = handler.handle(peer.clone(), body);
                        let outgoing = outgoing.clone();
                        tasks.spawn(async move {
                            let body = response.await;
                            let _ = outgoing.send(Frame::ClientResponse { id, body }).await;
                        });
                    }
                    Frame::HostResponse { id, body } => {
                        if let Some(sender) = peer.pending.lock().await.remove(&id) {
                            let _ = sender.send(body);
                        }
                    }
                    Frame::Ping { nonce } => {
                        if outgoing.send(Frame::Pong { nonce }).await.is_err() {
                            return Ok(());
                        }
                    }
                    _ => return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "unexpected frame from client",
                    )),
                }
            }
            event = events.recv() => {
                let frame = match event {
                    Ok((seq, body)) => Frame::Event { seq, body },
                    Err(broadcast::error::RecvError::Lagged(_)) => Frame::Resync,
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                };
                if outgoing.send(frame).await.is_err() {
                    return Ok(());
                }
            }
            _ = &mut idle => return Err(std::io::ErrorKind::TimedOut.into()),
            Some(_) = tasks.join_next() => {}
            Some(result) = writer.join_next() => {
                return match result {
                    Ok(result) => result,
                    Err(error) => Err(std::io::Error::other(error)),
                };
            }
        }
    }
}

fn check_hello(hello: &Hello, token: Option<&str>) -> Option<&'static str> {
    if hello.protocol != PROTOCOL_VERSION {
        return Some("protocol version mismatch");
    }
    let expected = token?;
    let offered = hello.token.as_deref().unwrap_or("");
    let matches: bool = offered.as_bytes().ct_eq(expected.as_bytes()).into();
    (!matches).then_some("invalid token")
}

/// The start of a client's name, for logs: the client chose it.
fn shown(client: &str) -> &str {
    client
        .char_indices()
        .nth(64)
        .map_or(client, |(end, _)| &client[..end])
}

async fn reject<P: Protocol>(wire: &mut Wire, reason: &str) -> std::io::Result<()> {
    frame::send::<P>(
        wire,
        &Frame::Rejected {
            reason: reason.into(),
        },
    )
    .await?;
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        reason,
    ))
}
