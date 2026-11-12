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

struct Authority {
    gateways: Vec<buzz_sdk::gateway::Live>,
    relay: Option<PublicKey>,
}

enum Cached {
    Empty,
    Fresh { at: Instant, authority: Authority },
    Failed { at: Instant, error: LookupError },
}

/// The owner's live gateways for one agent, from the relay, with a bounded
/// cache.
///
/// The cache mutex is held across the relay lookup. Lookups are therefore
/// single-flight: a slower, older lookup can never overwrite a newer result.
pub(crate) struct GatewayAccess {
    owner: PublicKey,
    agent: PublicKey,
    channel: uuid::Uuid,
    rest: RestClient,
    cached: tokio::sync::Mutex<Cached>,
}

impl GatewayAccess {
    /// Gateway access for `agent`, owned by `owner`, read through `rest`.
    pub(crate) fn new(
        owner: PublicKey,
        agent: PublicKey,
        rest: RestClient,
        channel: uuid::Uuid,
    ) -> Self {
        Self {
            owner,
            agent,
            channel,
            rest,
            cached: tokio::sync::Mutex::new(Cached::Empty),
        }
    }

    /// The live gateways now, newest delegation first, at most
    /// [`buzz_sdk::gateway::MAX_LIVE_GATEWAYS`].
    pub(crate) async fn live(&self) -> Result<Vec<PublicKey>, LookupError> {
        Ok(self.authority().await?.0)
    }

    async fn authority(&self) -> Result<(Vec<PublicKey>, Option<PublicKey>), LookupError> {
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
                Ok(Ok(authority)) => Cached::Fresh {
                    at: lookup_started,
                    authority,
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
            Cached::Fresh { authority, .. } => Ok((
                authority
                    .gateways
                    .iter()
                    .filter(|live| live.is_live_at(now))
                    .map(|live| live.gateway())
                    .collect(),
                authority.relay,
            )),
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

    /// Admit a direct gateway prompt, or a workflow prompt whose gateway owner
    /// is proven by the current relay's signature and explicit agent mention.
    /// Workflow routing tags alone never confer gateway authority.
    pub(crate) async fn prompt_author(
        &self,
        event: &Event,
    ) -> Result<Option<PublicKey>, LookupError> {
        let (gateways, relay) = self.authority().await?;
        if gateways.contains(&event.pubkey) {
            return Ok(Some(event.pubkey));
        }
        let Some(relay) = relay else {
            return Ok(None);
        };
        let Some(owner) = buzz_sdk::workflow::gateway_owner(event, relay, self.agent) else {
            return Ok(None);
        };
        if !gateways.contains(&owner)
            || !buzz_sdk::sharing::task::scope(event, self.agent, unix_now())
                .is_ok_and(|scope| scope.channel == self.channel)
        {
            return Ok(None);
        }
        Ok(Some(owner))
    }

    async fn lookup(&self) -> Result<Authority, LookupError> {
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
            return Ok(Authority {
                gateways: Vec::new(),
                relay: None,
            });
        }
        let profiles = self
            .query(json!({
                "kinds": [0],
                "authors": candidates,
                "limit": candidates.len(),
            }))
            .await?;
        let relay = self
            .rest
            .relay_self()
            .await
            .map_err(|error| LookupError(error.to_string()))?
            .and_then(|key| PublicKey::from_hex(&key).ok())
            .ok_or_else(|| LookupError("relay identity unavailable".into()))?;
        let rosters = self
            .query(json!({"kinds":[39002], "authors":[relay],
            "#d":[self.channel], "limit":1}))
            .await?;
        if rosters.len() > 1 {
            return Err(LookupError("ambiguous channel membership".into()));
        }
        let Some(roster) = rosters.first() else {
            return Ok(Authority {
                gateways: Vec::new(),
                relay: Some(relay),
            });
        };
        let member = |key| buzz_sdk::gateway::channel_member(roster, relay, self.channel, key);
        if !member(self.owner) || !member(self.agent) {
            return Ok(Authority {
                gateways: Vec::new(),
                relay: Some(relay),
            });
        }
        Ok(Authority {
            gateways: buzz_sdk::gateway::resolve(
                self.owner,
                &heads,
                &profiles,
                &[self.owner, self.agent],
                now,
            )
            .into_iter()
            .filter(|live| member(live.gateway()))
            .collect(),
            relay: Some(relay),
        })
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
