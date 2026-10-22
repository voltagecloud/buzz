//! What runs remote workspaces for a host: a [`SandboxBackend`] the host is
//! built with, which launches agents with what [`AgentCustody`] gives it.

use crate::api::{SandboxRequest, SandboxResponse};
use futures::future::BoxFuture;
use std::sync::Arc;
use zeroize::Zeroizing;

/// Serves [`SandboxRequest`]s. A host without one answers
/// [`SandboxResponse::unsupported`].
pub trait SandboxBackend: Send + Sync {
    /// Serves one request; `custody` launches the host's agents. Failures,
    /// with their codes, are [`SandboxResponse::Failed`].
    fn serve(
        &self,
        request: SandboxRequest,
        custody: Arc<dyn AgentCustody>,
    ) -> BoxFuture<'static, SandboxResponse>;
}

/// The host's agents, as a backend launches them.
pub trait AgentCustody: Send + Sync {
    /// The agent's launch identity. Reads its key from the credential store
    /// on first use, which may wait a few seconds on a busy store. Fails
    /// for an agent that is deleted or not on this host.
    fn launch_identity(&self, pubkey: &str) -> BoxFuture<'static, Result<LaunchIdentity, String>>;

    /// As [`Self::launch_identity`], but only with a key already read, so
    /// it never waits: for code that runs under a lock. Call
    /// [`Self::launch_identity`] first to read the key.
    fn cached_launch_identity(&self, pubkey: &str) -> Result<LaunchIdentity, String>;
}

/// What a provider needs to run an agent as itself. Never sent to a GUI.
#[derive(Clone)]
pub struct LaunchIdentity {
    pub pubkey: String,
    /// The canonical `wss://` community origin.
    pub relay: String,
    pub owner: String,
    /// The owner's NIP-OA `auth` tag, JSON encoded.
    pub auth: String,
    /// The agent's key, bech32 `nsec1…`.
    pub nsec: Zeroizing<String>,
}

impl std::fmt::Debug for LaunchIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchIdentity")
            .field("pubkey", &self.pubkey)
            .field("relay", &self.relay)
            .field("owner", &self.owner)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}
