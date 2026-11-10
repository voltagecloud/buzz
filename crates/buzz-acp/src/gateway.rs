//! Owner-delegated gateway keys, as the harness accepts them.
//!
//! A gateway is a separate key the agent's owner names in a signed, expiring
//! kind 30181 head (see `buzz_sdk::gateway` and `docs/agent-gateway-key.md`).
//! The harness accepts a live gateway in exactly three places, and only when
//! it runs in strict owner mode (`--respond-to owner`, which every remote
//! channel-thread sandbox session uses), with an immutable thread binding:
//! direct-message and unbound sessions never enable gateway access.
//!
//! 1. channel events at the inbound author gate (never DMs), rechecked again
//!    when a queued turn reaches model dispatch;
//! 2. `cancel_turn` observer control frames (no other control command);
//! 3. observer telemetry, which is also encrypted to each live gateway.
//!
//! Every lookup fails closed. Verdicts are cached for [`VERDICT_TTL`] so a
//! revocation takes effect on the next lookup after that, and a failure is
//! remembered for [`FAILURE_RETRY`] so a relay outage cannot turn every event
//! into an inline relay round-trip.

use std::time::Duration;

use nostr::{Event, PublicKey};
use serde_json::{json, Value};
use tokio::time::Instant;

use crate::relay::RestClient;

/// Longest a successful lookup is reused, which bounds revocation latency.
pub(crate) const VERDICT_TTL: Duration = Duration::from_secs(30);
/// Delay before a failed lookup is retried. Until then, gateways are refused.
pub(crate) const FAILURE_RETRY: Duration = Duration::from_secs(5);
/// Bound on one complete lookup (delegation heads plus profiles).
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// A delegation lookup failed; callers refuse the gateway.
#[derive(Debug, Clone, thiserror::Error)]
#[error("gateway delegation lookup failed: {0}")]
pub(crate) struct LookupError(String);

enum Cached {
    Empty,
    Fresh {
        at: Instant,
        gateways: Vec<buzz_sdk::gateway::Live>,
    },
    Failed {
        at: Instant,
        error: LookupError,
    },
}

/// The owner's live gateways for one agent, from the relay, with a bounded
/// cache.
///
/// The cache mutex is held across the relay lookup. Lookups are therefore
/// single-flight: a slower, older lookup can never overwrite a newer result.
pub(crate) struct GatewayAccess {
    owner: PublicKey,
    agent: PublicKey,
    rest: RestClient,
    cached: tokio::sync::Mutex<Cached>,
}

impl GatewayAccess {
    /// Gateway access for `agent`, owned by `owner`, read through `rest`.
    pub(crate) fn new(owner: PublicKey, agent: PublicKey, rest: RestClient) -> Self {
        Self {
            owner,
            agent,
            rest,
            cached: tokio::sync::Mutex::new(Cached::Empty),
        }
    }

    /// The live gateways now, newest delegation first, at most
    /// [`buzz_sdk::gateway::MAX_LIVE_GATEWAYS`].
    pub(crate) async fn live(&self) -> Result<Vec<PublicKey>, LookupError> {
        let mut cached = self.cached.lock().await;
        let reuse = match &*cached {
            Cached::Fresh { at, .. } => at.elapsed() < VERDICT_TTL,
            Cached::Failed { at, .. } => at.elapsed() < FAILURE_RETRY,
            Cached::Empty => false,
        };
        if !reuse {
            // Age successful results from lookup start so network latency
            // cannot extend the authority cache beyond the documented TTL.
            let lookup_started = Instant::now();
            *cached = match tokio::time::timeout(LOOKUP_TIMEOUT, self.lookup()).await {
                Ok(Ok(gateways)) => Cached::Fresh {
                    at: lookup_started,
                    gateways,
                },
                Ok(Err(error)) => Cached::Failed {
                    at: Instant::now(),
                    error,
                },
                Err(_) => Cached::Failed {
                    at: Instant::now(),
                    error: LookupError("timed out".into()),
                },
            };
        }
        let now = unix_now();
        match &*cached {
            // A cached delegation never outlives its own expiration.
            Cached::Fresh { gateways, .. } => Ok(gateways
                .iter()
                .filter(|live| live.is_live_at(now))
                .map(|live| live.gateway())
                .collect()),
            Cached::Failed { error, .. } => Err(error.clone()),
            Cached::Empty => Err(LookupError("no lookup".into())),
        }
    }

    /// Whether `key` is a live gateway of this agent's owner now.
    pub(crate) async fn is_live(&self, key: &PublicKey) -> Result<bool, LookupError> {
        if *key == self.owner || *key == self.agent {
            return Ok(false);
        }
        Ok(self.live().await?.contains(key))
    }

    async fn lookup(&self) -> Result<Vec<buzz_sdk::gateway::Live>, LookupError> {
        let now = unix_now();
        let heads = self
            .query(json!({
                "kinds": [buzz_sdk::kind::KIND_AGENT_GATEWAY_DELEGATION],
                "authors": [self.owner.to_hex()],
                "limit": buzz_sdk::gateway::LOOKUP_LIMIT,
            }))
            .await?;
        let candidates: Vec<String> = buzz_sdk::gateway::live_heads(self.owner, &heads, now)
            .iter()
            .map(|live| live.gateway().to_hex())
            .collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let profiles = self
            .query(json!({
                "kinds": [0],
                "authors": candidates,
                "limit": candidates.len(),
            }))
            .await?;
        Ok(buzz_sdk::gateway::resolve(
            self.owner,
            &heads,
            &profiles,
            &[self.owner, self.agent],
            now,
        ))
    }

    /// One `/query`, refusing any response that is not an array of events.
    async fn query(&self, filter: Value) -> Result<Vec<Event>, LookupError> {
        let mut filter = filter;
        filter["consistency"] = json!("strong");
        let response = self
            .rest
            .query_raw(&[filter])
            .await
            .map_err(|error| LookupError(error.to_string()))?;
        serde_json::from_value(response).map_err(|_| LookupError("malformed relay response".into()))
    }
}

fn unix_now() -> u64 {
    nostr::Timestamp::now().as_secs()
}

#[cfg(test)]
pub(crate) mod tests;
