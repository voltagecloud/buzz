//! Connection between the GUI and agent hosts.
//!
//! The crate is generic over a [`Protocol`]: the request, response and event
//! types that travel over the connection. See `ARCHITECTURE.md` for the wire
//! design.

mod client;
mod frame;
mod link;
mod server;
pub mod transport;

pub use client::{ClientEvent, ClientHandler, ConnectionState, HostClient, HostClientOptions};
pub use frame::{HostInfo, PROTOCOL_VERSION};
pub use server::{EventSource, HostHandler, Peer, ServerOptions, serve};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub trait Message: Serialize + DeserializeOwned + Send + Sync + 'static {}
impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> Message for T {}

pub trait Protocol: Send + Sync + 'static {
    /// Requests from the GUI to the host.
    type ClientRequest: Message;
    type ClientResponse: Message;
    /// Requests from the host to the GUI, e.g. asking the human to sign.
    type HostRequest: Message;
    type HostResponse: Message;
    /// State changes the host pushes to every connected GUI.
    type Event: Message + Clone;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum RpcError {
    #[error("not connected to the host")]
    NotConnected,
    #[error("the connection closed before the request finished")]
    Disconnected,
    #[error("the request timed out")]
    TimedOut,
    #[error("{0}")]
    Failed(String),
}

impl RpcError {
    /// The other side could not read the request: it predates it.
    pub fn is_unsupported(&self) -> bool {
        *self == frame::unsupported()
    }
}
