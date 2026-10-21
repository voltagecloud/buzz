use crate::transport::BoxConnection;
use crate::{Message, Protocol, RpcError};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::io;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// Bumped on any change the other side cannot ignore. Adding optional fields,
/// requests or events does not need a bump: a peer answers a request it
/// cannot read with an error and skips an event it cannot read.
pub const PROTOCOL_VERSION: u32 = 1;

pub(crate) const MAX_FRAME: usize = 16 << 20;
/// The limit until the handshake is done, so a peer that has not yet
/// shown its token cannot make this side buffer a large frame.
const HANDSHAKE_FRAME: usize = 64 << 10;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub token: Option<String>,
    pub client: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// Stable for the lifetime of the host's state directory.
    pub id: String,
    pub name: String,
    pub platform: String,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: u32,
    pub host: HostInfo,
}

#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
pub(crate) enum Frame<P: Protocol> {
    Hello(Hello),
    Welcome(Welcome),
    Rejected {
        reason: String,
    },
    ClientRequest {
        id: u64,
        body: P::ClientRequest,
    },
    ClientResponse {
        id: u64,
        body: Result<P::ClientResponse, RpcError>,
    },
    HostRequest {
        id: u64,
        body: P::HostRequest,
    },
    HostResponse {
        id: u64,
        body: Result<P::HostResponse, RpcError>,
    },
    Event {
        seq: u64,
        body: P::Event,
    },
    /// The host dropped events for this client; it must fetch fresh state.
    Resync,
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

pub(crate) type Wire = Framed<BoxConnection, LengthDelimitedCodec>;

/// A connection that takes frames up to the handshake's limit, until
/// [`handshaken`] raises it.
pub(crate) fn wire(connection: BoxConnection) -> Wire {
    Framed::new(
        connection,
        LengthDelimitedCodec::builder()
            .max_frame_length(HANDSHAKE_FRAME)
            .new_codec(),
    )
}

pub(crate) fn handshaken(wire: &mut Wire) {
    wire.codec_mut().set_max_frame_length(MAX_FRAME);
}

pub(crate) fn encode<T: Message>(value: &T) -> io::Result<Bytes> {
    rmp_serde::to_vec_named(value)
        .map(Bytes::from)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn decode<T: Message>(bytes: &[u8]) -> io::Result<T> {
    rmp_serde::from_slice(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// What can still be read from a frame that did not decode, typically
/// because a newer peer sent a request or event this side does not know.
pub(crate) enum Unknown {
    /// A request; answered with an error so the connection stays up.
    Request(u64),
    /// An event; skipped.
    Event,
}

/// Frames are maps keyed by their variant, so the envelope of a request or
/// event decodes whatever its body is.
pub(crate) fn peek(bytes: &[u8]) -> Option<Unknown> {
    #[derive(Deserialize)]
    struct Id {
        id: u64,
    }
    #[derive(Deserialize)]
    #[allow(non_snake_case)]
    struct Envelope {
        ClientRequest: Option<Id>,
        HostRequest: Option<Id>,
        Event: Option<serde::de::IgnoredAny>,
    }
    let envelope: Envelope = rmp_serde::from_slice(bytes).ok()?;
    match envelope {
        Envelope {
            ClientRequest: Some(Id { id }),
            ..
        }
        | Envelope {
            HostRequest: Some(Id { id }),
            ..
        } => Some(Unknown::Request(id)),
        Envelope { Event: Some(_), .. } => Some(Unknown::Event),
        _ => None,
    }
}

pub(crate) fn unsupported() -> RpcError {
    RpcError::Failed("The other side does not support this request; update it.".into())
}

pub(crate) async fn send<P: Protocol>(wire: &mut Wire, frame: &Frame<P>) -> io::Result<()> {
    wire.send(encode(frame)?).await
}

pub(crate) async fn receive<P: Protocol>(wire: &mut Wire) -> io::Result<Frame<P>> {
    match wire.next().await {
        Some(bytes) => decode(&bytes?),
        None => Err(io::ErrorKind::UnexpectedEof.into()),
    }
}
