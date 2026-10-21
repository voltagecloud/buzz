//! Byte-stream transports between a GUI and an agent host.
//!
//! A transport only has to produce an ordered, reliable byte stream. Framing,
//! authentication and reconnection live above it, so adding a transport (SSH,
//! a relay tunnel, ...) means implementing [`Connector`] and [`Listener`].

use futures::future::BoxFuture;
use std::io;
#[cfg(unix)]
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, mpsc};

pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Connection for T {}

pub type BoxConnection = Box<dyn Connection>;

/// Client side: opens a new connection each time it is called, so the client
/// can reconnect after a failure.
pub trait Connector: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'_, io::Result<BoxConnection>>;
    /// Human-readable target, e.g. `tcp://devbox:7420`.
    fn describe(&self) -> String;
}

/// Server side: yields accepted connections. An error of kind
/// [`io::ErrorKind::BrokenPipe`] means the listener has closed for good;
/// any other fails only that accept.
pub trait Listener: Send + 'static {
    fn accept(&mut self) -> BoxFuture<'_, io::Result<(BoxConnection, String)>>;
    fn describe(&self) -> String;
}

pub struct TcpConnector {
    pub address: String,
}

impl Connector for TcpConnector {
    fn connect(&self) -> BoxFuture<'_, io::Result<BoxConnection>> {
        Box::pin(async move {
            let stream = tokio::net::TcpStream::connect(&self.address).await?;
            stream.set_nodelay(true)?;
            Ok(Box::new(stream) as BoxConnection)
        })
    }

    fn describe(&self) -> String {
        format!("tcp://{}", self.address)
    }
}

pub struct TcpListener {
    inner: tokio::net::TcpListener,
}

impl TcpListener {
    pub async fn bind(address: &str) -> io::Result<Self> {
        Ok(Self {
            inner: tokio::net::TcpListener::bind(address).await?,
        })
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }
}

impl Listener for TcpListener {
    fn accept(&mut self) -> BoxFuture<'_, io::Result<(BoxConnection, String)>> {
        Box::pin(async move {
            let (stream, peer) = self.inner.accept().await?;
            // Only latency: a socket the peer already reset fails on read.
            let _ = stream.set_nodelay(true);
            Ok((Box::new(stream) as BoxConnection, peer.to_string()))
        })
    }

    fn describe(&self) -> String {
        match self.inner.local_addr() {
            Ok(address) => format!("tcp://{address}"),
            Err(_) => "tcp://?".into(),
        }
    }
}

#[cfg(unix)]
pub struct UnixConnector {
    pub path: PathBuf,
}

#[cfg(unix)]
impl Connector for UnixConnector {
    fn connect(&self) -> BoxFuture<'_, io::Result<BoxConnection>> {
        Box::pin(async move {
            let stream = tokio::net::UnixStream::connect(&self.path).await?;
            Ok(Box::new(stream) as BoxConnection)
        })
    }

    fn describe(&self) -> String {
        format!("unix://{}", self.path.display())
    }
}

#[cfg(unix)]
pub struct UnixListener {
    inner: tokio::net::UnixListener,
    path: PathBuf,
}

#[cfg(unix)]
impl UnixListener {
    /// Binds a socket readable and writable only by the current user. A stale
    /// socket file from a previous run is replaced.
    pub fn bind(path: PathBuf) -> io::Result<Self> {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(&path)?,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let inner = tokio::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { inner, path })
    }
}

#[cfg(unix)]
impl Listener for UnixListener {
    fn accept(&mut self) -> BoxFuture<'_, io::Result<(BoxConnection, String)>> {
        Box::pin(async move {
            let (stream, _) = self.inner.accept().await?;
            Ok((Box::new(stream) as BoxConnection, "unix".to_string()))
        })
    }

    fn describe(&self) -> String {
        format!("unix://{}", self.path.display())
    }
}

#[cfg(unix)]
impl Drop for UnixListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// In-process transport, used for the host embedded in the GUI and in tests.
pub fn memory() -> (MemoryConnector, MemoryListener) {
    let (sender, receiver) = mpsc::channel(4);
    (
        MemoryConnector { sender },
        MemoryListener {
            receiver: Mutex::new(receiver),
        },
    )
}

const MEMORY_BUFFER: usize = 1 << 20;

pub struct MemoryConnector {
    sender: mpsc::Sender<tokio::io::DuplexStream>,
}

impl Connector for MemoryConnector {
    fn connect(&self) -> BoxFuture<'_, io::Result<BoxConnection>> {
        Box::pin(async move {
            let (client, server) = tokio::io::duplex(MEMORY_BUFFER);
            self.sender
                .send(server)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::ConnectionRefused, "host stopped"))?;
            Ok(Box::new(client) as BoxConnection)
        })
    }

    fn describe(&self) -> String {
        "memory".into()
    }
}

pub struct MemoryListener {
    receiver: Mutex<mpsc::Receiver<tokio::io::DuplexStream>>,
}

impl Listener for MemoryListener {
    fn accept(&mut self) -> BoxFuture<'_, io::Result<(BoxConnection, String)>> {
        Box::pin(async move {
            match self.receiver.lock().await.recv().await {
                Some(stream) => Ok((Box::new(stream) as BoxConnection, "memory".to_string())),
                None => Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "all connectors dropped",
                )),
            }
        })
    }

    fn describe(&self) -> String {
        "memory".into()
    }
}
