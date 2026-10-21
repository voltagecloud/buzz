//! One running app per profile. A second launch hands its `buzz://` URL
//! (or nothing, just to be focused) and its activation token to the
//! running instance and exits: over a Unix socket on Linux and macOS, over
//! a named pipe on Windows.
//!
//! The socket is keyed by the data directory, so separate profiles (and
//! development runs with their own `XDG_DATA_HOME`) never capture each
//! other's launches. On Linux it lives in `$XDG_RUNTIME_DIR`, which only
//! the user can enter; without one it falls back to the data directory.
//! On macOS it lives in the user's own `$TMPDIR`, which keeps the path
//! within the socket address limit. On Windows the data directory is in
//! the user's profile, so the pipe's name is the user's own too.

use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
pub use unix::{Listener, claim, release, serve};
#[cfg(windows)]
pub use windows::{Listener, claim, release, serve};

/// Longest message accepted: a URL as long as the link parser takes.
const MAX_MESSAGE: u64 = 32_768;
const TIMEOUT: Duration = Duration::from_secs(2);

/// What a second launch hands over: a line with the link, then a line
/// with the activation token, if any. Instances that predate the token
/// read the first line only.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Handoff {
    /// Empty to only bring the window to the front.
    pub link: String,
    pub activation_token: Option<String>,
}

pub enum Claim {
    /// This is the running instance; serve the socket.
    Primary(Listener),
    /// Another instance took the message.
    Forwarded,
}

/// The socket for the profile stored in `data_dir`.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    let name = format!(
        "buzz-gpui-{:016x}",
        fnv1a(data_dir.as_os_str().as_encoded_bytes())
    );
    if cfg!(windows) {
        return PathBuf::from(format!(r"\\.\pipe\{name}"));
    }
    let base = if cfg!(target_os = "macos") {
        std::env::temp_dir()
    } else {
        dirs::runtime_dir().unwrap_or_else(|| data_dir.to_path_buf())
    };
    base.join(format!("{name}.sock"))
}

/// A hash that stays the same across builds, so differently built copies
/// still find each other.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The line or two a second launch writes, each ending in a line break.
fn message(handoff: &Handoff) -> String {
    let mut message = single_line(&handoff.link);
    if let Some(token) = &handoff.activation_token {
        message.push('\n');
        message.push_str(&single_line(token));
    }
    message.push('\n');
    message
}

/// A field may not break the line-based message.
fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], "")
}

fn read_message(reader: impl Read) -> io::Result<Handoff> {
    let mut reader = BufReader::new(reader.take(MAX_MESSAGE + 1));
    let mut link = String::new();
    reader.read_line(&mut link)?;
    let mut token = String::new();
    reader.read_line(&mut token)?;
    if (link.len() + token.len()) as u64 > MAX_MESSAGE {
        return Err(io::Error::other("message too long"));
    }
    let token = token.trim_end_matches(['\r', '\n']);
    Ok(Handoff {
        link: link.trim_end_matches(['\r', '\n']).to_string(),
        activation_token: (!token.is_empty()).then(|| token.to_string()),
    })
}

#[cfg(unix)]
mod unix {
    use super::{Claim, Handoff, TIMEOUT, message, read_message};
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;

    pub type Listener = UnixListener;

    /// Held while a launch claims the socket or the running instance
    /// removes it, so two launches never both find it stale and both bind.
    struct ClaimLock(File);

    impl ClaimLock {
        fn take(path: &Path) -> io::Result<Self> {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(path.with_extension("lock"))?;
            file.lock()?;
            Ok(Self(file))
        }
    }

    /// Lets go explicitly: a process spawned meanwhile holds a copy of the
    /// descriptor until it execs.
    impl Drop for ClaimLock {
        fn drop(&mut self) {
            let _ = self.0.unlock();
        }
    }

    /// Becomes the running instance, or hands `handoff` to the one that is.
    pub fn claim(path: &Path, handoff: &Handoff) -> io::Result<Claim> {
        let message = message(handoff);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _lock = ClaimLock::take(path)?;
        for _ in 0..2 {
            match UnixListener::bind(path) {
                Ok(listener) => return Ok(Claim::Primary(listener)),
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                    match UnixStream::connect(path) {
                        Ok(mut stream) => {
                            stream.set_write_timeout(Some(TIMEOUT))?;
                            stream.write_all(message.as_bytes())?;
                            return Ok(Claim::Forwarded);
                        }
                        // Left behind by an instance that exited.
                        Err(_) => std::fs::remove_file(path)?,
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other("the instance socket stayed in use"))
    }

    /// Removes the running instance's socket as it exits.
    pub fn release(path: &Path) {
        match ClaimLock::take(path) {
            Ok(_lock) => {
                let _ = std::fs::remove_file(path);
            }
            Err(error) => tracing::warn!(%error, "could not remove the instance socket"),
        }
    }

    /// Passes each handoff from later launches to `deliver`, on a thread of
    /// its own, for as long as the process runs.
    pub fn serve(listener: UnixListener, deliver: impl Fn(Handoff) + Send + 'static) {
        let spawned = std::thread::Builder::new()
            .name("buzz-instance".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let message = stream.and_then(|stream| {
                        stream.set_read_timeout(Some(TIMEOUT))?;
                        read_message(stream)
                    });
                    match message {
                        Ok(message) => deliver(message),
                        Err(error) => tracing::warn!(%error, "could not read from a second launch"),
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not listen for second launches");
        }
    }
}

/// The same over a named pipe. A pipe goes away with the last process that
/// has it open, so there is never a stale one to replace.
#[cfg(windows)]
mod windows {
    use super::{Claim, Handoff, MAX_MESSAGE, TIMEOUT, message, read_message};
    use std::io;
    use std::path::Path;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use tokio::runtime::Runtime;

    /// What `CreateNamedPipe` reports when another process made the first
    /// instance, and what `CreateFile` reports while every instance is
    /// connected to someone else.
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_PIPE_BUSY: i32 = 231;

    /// The first instance of the pipe, and the runtime that drives it.
    pub struct Listener {
        name: String,
        server: NamedPipeServer,
        runtime: Runtime,
    }

    /// Becomes the running instance, or hands `handoff` to the one that is.
    pub fn claim(path: &Path, handoff: &Handoff) -> io::Result<Claim> {
        let name = path.to_string_lossy().into_owned();
        let message = message(handoff);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        for _ in 0..2 {
            let created = {
                let _entered = runtime.enter();
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .reject_remote_clients(true)
                    .create(&name)
            };
            match created {
                Ok(server) => {
                    return Ok(Claim::Primary(Listener {
                        name,
                        server,
                        runtime,
                    }));
                }
                Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                    match runtime.block_on(forward(&name, &message)) {
                        Ok(()) => return Ok(Claim::Forwarded),
                        // It exited between the two calls.
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other("the instance pipe stayed in use"))
    }

    /// The pipe goes with the process; nothing to remove.
    pub fn release(_: &Path) {}

    async fn forward(name: &str, message: &str) -> io::Result<()> {
        let until = Instant::now() + TIMEOUT;
        let mut client = loop {
            match ClientOptions::new().open(name) {
                Ok(client) => break client,
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < until =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(error) => return Err(error),
            }
        };
        tokio::time::timeout(TIMEOUT, client.write_all(message.as_bytes()))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
    }

    /// Passes each handoff from later launches to `deliver`, on a thread of
    /// its own, for as long as the process runs.
    pub fn serve(listener: Listener, deliver: impl Fn(Handoff) + Send + 'static) {
        let Listener {
            name,
            mut server,
            runtime,
        } = listener;
        let spawned = std::thread::Builder::new()
            .name("buzz-instance".into())
            .spawn(move || {
                runtime.block_on(async move {
                    loop {
                        if let Err(error) = server.connect().await {
                            tracing::warn!(%error, "could not accept a second launch");
                        }
                        // The next launch finds a fresh instance while this
                        // one is read.
                        let next = match ServerOptions::new().create(&name) {
                            Ok(next) => next,
                            Err(error) => {
                                tracing::warn!(%error, "stopped listening for second launches");
                                return;
                            }
                        };
                        let connected = std::mem::replace(&mut server, next);
                        let message = tokio::time::timeout(TIMEOUT, read_lines(connected))
                            .await
                            .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
                            .and_then(|bytes| read_message(bytes.as_slice()));
                        match message {
                            Ok(message) => deliver(message),
                            Err(error) => {
                                tracing::warn!(%error, "could not read from a second launch")
                            }
                        }
                    }
                })
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not listen for second launches");
        }
    }

    /// Reads until the client closes the pipe or has sent both lines.
    async fn read_lines(mut pipe: NamedPipeServer) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        while bytes.iter().filter(|byte| **byte == b'\n').count() < 2
            && bytes.len() as u64 <= MAX_MESSAGE
        {
            match pipe.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
                Err(error) => return Err(error),
            }
        }
        Ok(bytes)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::{UnixDatagram, UnixStream};
    use std::sync::mpsc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "buzz-instance-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("s.sock")
    }

    #[test]
    fn second_claim_forwards_to_the_first() {
        let path = scratch("forward");
        let Claim::Primary(listener) = claim(&path, &Handoff::default()).unwrap() else {
            panic!("first claim should be primary");
        };
        let (sender, received) = mpsc::channel();
        serve(listener, move |message| sender.send(message).unwrap());
        let link = Handoff {
            link: "buzz://channel/general".into(),
            activation_token: Some("token\nwith a break".into()),
        };
        assert!(matches!(claim(&path, &link).unwrap(), Claim::Forwarded));
        assert!(matches!(
            claim(&path, &Handoff::default()).unwrap(),
            Claim::Forwarded
        ));
        let wait = Duration::from_secs(5);
        assert_eq!(
            received.recv_timeout(wait).unwrap(),
            Handoff {
                link: "buzz://channel/general".into(),
                activation_token: Some("tokenwith a break".into()),
            }
        );
        assert_eq!(received.recv_timeout(wait).unwrap(), Handoff::default());
    }

    #[test]
    fn reads_handoffs_without_a_token() {
        let path = scratch("legacy");
        let Claim::Primary(listener) = claim(&path, &Handoff::default()).unwrap() else {
            panic!("first claim should be primary");
        };
        let (sender, received) = mpsc::channel();
        serve(listener, move |message| sender.send(message).unwrap());
        // What an older launch writes.
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"buzz://channel/general\n").unwrap();
        drop(stream);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            Handoff {
                link: "buzz://channel/general".into(),
                activation_token: None,
            }
        );
    }

    #[test]
    fn replaces_a_stale_socket() {
        let path = scratch("stale");
        // A datagram socket leaves the same file and always refuses a
        // stream connection. A dropped listener would not: a process
        // another test spawns at that moment holds a copy of it until it
        // execs, and the connection gets through.
        drop(UnixDatagram::bind(&path).unwrap());
        assert!(path.exists());
        assert!(matches!(
            claim(&path, &Handoff::default()).unwrap(),
            Claim::Primary(_)
        ));
    }

    #[test]
    fn launches_racing_over_a_stale_socket_get_one_primary() {
        let path = scratch("race");
        drop(UnixDatagram::bind(&path).unwrap());
        let claims: Vec<Claim> = std::thread::scope(|scope| {
            let launches: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| claim(&path, &Handoff::default()).unwrap()))
                .collect();
            launches.into_iter().map(|l| l.join().unwrap()).collect()
        });
        let primaries = claims
            .iter()
            .filter(|claim| matches!(claim, Claim::Primary(_)))
            .count();
        assert_eq!(primaries, 1);
        release(&path);
        assert!(!path.exists());
    }

    #[test]
    fn profiles_get_their_own_socket() {
        let a = socket_path(Path::new("/home/a/.local/share/buzz-gpui"));
        let b = socket_path(Path::new("/tmp/t/data/buzz-gpui"));
        assert_ne!(a, b);
        assert_eq!(a, socket_path(Path::new("/home/a/.local/share/buzz-gpui")));
        assert!(a.to_string_lossy().ends_with(".sock"));
    }
}
