//! Owner-delegated gateway keys for driving the owner's own remote agents.
//!
//! A gateway is a separate key, for example an always-on orchestrator, that
//! the owner explicitly delegates with a signed, expiring, revocable kind
//! [`KIND_AGENT_GATEWAY_DELEGATION`] head. A live delegation lets the gateway
//! do exactly what [`Scope::RemoteAgents`] names: mention-launch the owner's
//! remote agents, prompt them in their channel threads, cancel their turns,
//! and read their observer telemetry. Shared-task admission
//! ([`crate::sharing::policy`]) judges a live gateway as its owner only when
//! the caller binds that gateway key to that owner (the session manager's own
//! Paseo daemon keys). The gateway then gets exactly the owner's sharing
//! permissions there, including teammates' agents that are shared in the
//! channel. A delegation never confers ownership, agent identity, or channel
//! or relay membership.
//!
//! Gateway authority and agent authority are disjoint by construction:
//!
//! - A gateway is named only by an owner-signed delegation head. A NIP-OA
//!   `auth` attestation, which every owned agent carries, is never gateway
//!   authority, and a delegation head is never agent authority.
//! - A key whose kind:0 profile carries any `auth` tag claims to be an agent,
//!   so [`resolve`] refuses it as a gateway even when a delegation names it.
//!   Consumers additionally refuse the owner and the consuming agent itself,
//!   and the relay refuses any key it has recorded as an agent.
//!
//! Every verdict fails closed: a malformed, expired, revoked, future-dated, or
//! over-long head is "not delegated", and callers treat a lookup failure the
//! same way. Only [`live`] and [`resolve`] construct a [`Live`] value, so code
//! holding one holds proof of a verified owner signature.

use std::collections::BTreeMap;

use nostr::{Event, EventBuilder, Kind, PublicKey, Tag, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{kind::KIND_AGENT_GATEWAY_DELEGATION, SdkError};

/// Longest delegation lifetime, measured from the head's `created_at`.
pub const MAX_DELEGATION_SECS: u64 = 90 * 24 * 60 * 60;

/// Most live gateways one owner may have. Consumers act on at most this many,
/// newest first, and the harness encrypts each telemetry frame once per live
/// gateway, so the cap also bounds telemetry fan-out.
pub const MAX_LIVE_GATEWAYS: usize = 2;

/// Largest accepted clock skew for a head's `created_at` in the future.
pub const MAX_FUTURE_SKEW_SECS: u64 = 60;

/// Most heads a consumer reads per owner in one lookup. Live heads beyond this
/// window are not seen, which fails closed.
pub const LOOKUP_LIMIT: usize = 32;

/// What a delegation permits. Unknown scopes do not deserialize, so a future
/// scope can never widen an older consumer's acceptance.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum Scope {
    /// Drive the signer's own remote agents: thread prompts, mention launch,
    /// `cancel_turn` control frames, and observer telemetry.
    #[serde(rename = "remote-agents")]
    RemoteAgents,
}

/// The signed content of one delegation head.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Delegation {
    /// Wire version; currently one.
    pub version: u32,
    /// The delegated gateway key.
    pub gateway: PublicKey,
    /// The only permitted scope.
    pub scope: Scope,
    /// `false` is a durable revocation head.
    pub enabled: bool,
}

/// A verified, unexpired delegation from `owner` to `gateway`.
///
/// Fields are private: the only constructors are [`live`] and [`resolve`],
/// which verify the owner's signature and the head's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Live {
    owner: PublicKey,
    gateway: PublicKey,
    expires_at: u64,
    created_at: u64,
}

impl Live {
    /// The delegating owner (the head's signer).
    pub fn owner(&self) -> PublicKey {
        self.owner
    }

    /// The delegated gateway key.
    pub fn gateway(&self) -> PublicKey {
        self.gateway
    }

    /// NIP-40 expiration, unix seconds.
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// The head's `created_at`, unix seconds.
    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    /// Whether this delegation is still unexpired at `now`.
    pub fn is_live_at(&self, now: u64) -> bool {
        self.expires_at > now
    }
}

impl Delegation {
    /// Build a grant head for `gateway` that expires at `expires_at`.
    /// The caller signs with the owner's key and must set `created_at` newer
    /// than any existing head for the same gateway.
    pub fn grant(
        gateway: PublicKey,
        expires_at: u64,
        created_at: u64,
    ) -> Result<EventBuilder, SdkError> {
        if !lifetime_ok(created_at, expires_at) {
            return Err(SdkError::InvalidInput(format!(
                "gateway delegation must expire within {} days",
                MAX_DELEGATION_SECS / 86_400
            )));
        }
        Self::build(gateway, true, Some(expires_at), created_at)
    }

    /// Build a revocation head for `gateway`. The caller signs with the owner's
    /// key and must set `created_at` newer than the current head.
    pub fn revoke(gateway: PublicKey, created_at: u64) -> Result<EventBuilder, SdkError> {
        Self::build(gateway, false, None, created_at)
    }

    fn build(
        gateway: PublicKey,
        enabled: bool,
        expires_at: Option<u64>,
        created_at: u64,
    ) -> Result<EventBuilder, SdkError> {
        let content = serde_json::to_string(&Self {
            version: 1,
            gateway,
            scope: Scope::RemoteAgents,
            enabled,
        })
        .map_err(|e| SdkError::InvalidInput(e.to_string()))?;
        let mut tags = vec![
            vec!["d".to_string(), gateway.to_hex()],
            vec!["p".to_string(), gateway.to_hex()],
        ];
        if let Some(expires_at) = expires_at {
            tags.push(vec!["expiration".to_string(), expires_at.to_string()]);
        }
        let tags = tags
            .into_iter()
            .map(Tag::parse)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| SdkError::InvalidTag(e.to_string()))?;
        Ok(
            EventBuilder::new(Kind::Custom(KIND_AGENT_GATEWAY_DELEGATION as u16), content)
                .tags(tags)
                .custom_created_at(Timestamp::from(created_at)),
        )
    }

    /// Verify the signature, kind, version, unambiguous signed scope, and
    /// lifetime shape of a head. Returns the content and its expiration.
    ///
    /// An enabled head must carry exactly one canonical `expiration` after its
    /// `created_at` and within [`MAX_DELEGATION_SECS`]; a revocation head must
    /// carry none, so no NIP-40 purge can ever remove a revocation. Liveness at
    /// a given time is checked separately by [`live`].
    pub fn parse(event: &Event) -> Result<(Self, Option<u64>), SdkError> {
        if u32::from(event.kind.as_u16()) != KIND_AGENT_GATEWAY_DELEGATION
            || event.verify().is_err()
        {
            return Err(invalid("invalid delegation signature or kind"));
        }
        let delegation: Self = serde_json::from_str(&event.content)
            .map_err(|_| invalid("invalid delegation content"))?;
        if delegation.version != 1 || delegation.gateway == event.pubkey {
            return Err(invalid("invalid delegation version or gateway"));
        }
        let gateway = delegation.gateway.to_hex();
        for name in ["d", "p"] {
            match single(event, name)? {
                Some(value) if value == gateway => {}
                _ => return Err(invalid("ambiguous delegation scope")),
            }
        }
        let expiration = match single(event, "expiration")? {
            Some(text) => Some(canonical_u64(text)?),
            None => None,
        };
        match (delegation.enabled, expiration) {
            (true, Some(expires_at)) if lifetime_ok(event.created_at.as_secs(), expires_at) => {}
            (true, _) => {
                return Err(invalid(
                    "an enabled delegation requires a bounded expiration",
                ))
            }
            (false, None) => {}
            (false, Some(_)) => {
                return Err(invalid("a revocation must not carry an expiration"));
            }
        }
        Ok((delegation, expiration))
    }
}

/// The live delegation `event` carries from `owner` to `gateway` at `now`, or
/// `None`. Expired, revoked, future-dated, and over-long heads are not live.
///
/// This judges one head in isolation. Use [`resolve`] over relay results so a
/// newer revocation for the same coordinate always wins.
pub fn live(event: &Event, owner: PublicKey, gateway: PublicKey, now: u64) -> Option<Live> {
    if event.pubkey != owner {
        return None;
    }
    let (delegation, expiration) = Delegation::parse(event).ok()?;
    let expires_at = expiration?;
    let created_at = event.created_at.as_secs();
    let live = delegation.enabled
        && delegation.gateway == gateway
        && created_at <= now.saturating_add(MAX_FUTURE_SKEW_SECS)
        && expires_at > now;
    live.then_some(Live {
        owner,
        gateway,
        expires_at,
        created_at,
    })
}

/// Whether a kind:0 profile claims agent identity: any `auth` tag at all.
/// Such a key is never accepted as a gateway, whoever the claimed owner is.
pub fn profile_claims_agent(profile: &Event) -> bool {
    profile
        .tags
        .iter()
        .any(|tag| tag.as_slice().first().map(String::as_str) == Some("auth"))
}

/// `owner`'s live delegations in `delegations`, before the agent-identity and
/// exclusion checks of [`resolve`]: newest first, at most
/// [`MAX_LIVE_GATEWAYS`]. Callers use the gateway keys to fetch the kind:0
/// profiles [`resolve`] needs.
///
/// For each coordinate (the first `d` tag, as NIP-33 addresses it) only the
/// newest correctly signed head counts, ties to the lowest id. That head must
/// itself parse and be live: a stale grant returned beside a newer revocation
/// or a newer malformed head never wins.
pub fn live_heads(owner: PublicKey, delegations: &[Event], now: u64) -> Vec<Live> {
    let mut heads: BTreeMap<&str, &Event> = BTreeMap::new();
    for event in delegations {
        if event.pubkey != owner
            || u32::from(event.kind.as_u16()) != KIND_AGENT_GATEWAY_DELEGATION
            || event.verify().is_err()
        {
            continue;
        }
        let Some(coordinate) = first_d_tag(event) else {
            continue;
        };
        let newer = heads.get(coordinate).is_none_or(|head| {
            (event.created_at, std::cmp::Reverse(event.id))
                > (head.created_at, std::cmp::Reverse(head.id))
        });
        if newer {
            heads.insert(coordinate, event);
        }
    }
    let mut live: Vec<Live> = heads
        .values()
        .filter_map(|head| {
            let (delegation, _) = Delegation::parse(head).ok()?;
            live(head, owner, delegation.gateway, now)
        })
        .collect();
    live.sort_by_key(|live| (std::cmp::Reverse(live.created_at), live.gateway.to_hex()));
    live.truncate(MAX_LIVE_GATEWAYS);
    live
}

/// Resolve `owner`'s live gateways from relay results.
///
/// Starts from [`live_heads`], then refuses gateways in `excluded` (the owner,
/// the consuming agent) and any key whose correctly signed kind:0 in
/// `profiles` claims agent identity. Refused keys still occupy their slot in
/// the [`MAX_LIVE_GATEWAYS`] window, so every consumer acts on the same set.
pub fn resolve(
    owner: PublicKey,
    delegations: &[Event],
    profiles: &[Event],
    excluded: &[PublicKey],
    now: u64,
) -> Vec<Live> {
    let claims_agent = |gateway: &PublicKey| {
        profiles.iter().any(|profile| {
            profile.pubkey == *gateway
                && profile.kind == Kind::Metadata
                && profile.verify().is_ok()
                && profile_claims_agent(profile)
        })
    };
    live_heads(owner, delegations, now)
        .into_iter()
        .filter(|live| !excluded.contains(&live.gateway) && !claims_agent(&live.gateway))
        .collect()
}

/// Read `owner`'s live gateways from the relay through `query`, then
/// [`resolve`] them with `excluded`.
///
/// This is the one delegation lookup every relay-reading consumer shares: up to
/// [`LOOKUP_LIMIT`] of the owner's heads, then the kind:0 profiles of the live
/// candidates so agent keys are refused. `query` receives NIP-01 filters that
/// carry a `limit`; it must read the relay's writer (`"consistency":"strong"`)
/// so a lagging replica cannot hide a revocation. Membership is not checked.
pub async fn lookup<F, Fut, E>(
    mut query: F,
    owner: PublicKey,
    excluded: &[PublicKey],
    now: u64,
) -> Result<Vec<Live>, E>
where
    F: FnMut(serde_json::Value) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Event>, E>>,
{
    let heads = query(serde_json::json!({
        "kinds": [KIND_AGENT_GATEWAY_DELEGATION],
        "authors": [owner.to_hex()],
        "limit": LOOKUP_LIMIT,
    }))
    .await?;
    let candidates: Vec<String> = live_heads(owner, &heads, now)
        .iter()
        .map(|live| live.gateway.to_hex())
        .collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let profiles = query(serde_json::json!({
        "kinds": [0],
        "authors": candidates,
        "limit": candidates.len(),
    }))
    .await?;
    Ok(resolve(owner, &heads, &profiles, excluded, now))
}

/// Build and sign an owner delegation with the current timestamp.
///
/// `expires_at` is a Unix timestamp, at most 90 days from now. The caller
/// submits this event to its community relay with the owner's identity.
pub fn build_gateway_grant(
    owner_keys: &nostr::Keys,
    gateway: PublicKey,
    expires_at: u64,
) -> Result<Event, SdkError> {
    if owner_keys.public_key() == gateway {
        return Err(invalid("the gateway must be a separate key"));
    }
    Delegation::grant(gateway, expires_at, Timestamp::now().as_secs())?
        .sign_with_keys(owner_keys)
        .map_err(|_| invalid("could not sign gateway delegation"))
}

/// Whether a signer has a live verified owner delegation in these relay
/// results. Supply current gateway profiles so agent identities are excluded.
/// Consumers must also enforce community and channel membership separately.
pub fn is_live_delegate(
    owner: PublicKey,
    signer: PublicKey,
    grants: &[Event],
    profiles: &[Event],
    now: u64,
) -> bool {
    resolve(owner, grants, profiles, &[owner], now)
        .iter()
        .any(|grant| grant.gateway() == signer)
}

/// Whether `key` has current posting membership in a relay-signed channel
/// roster (kind39002). Callers must fetch the newest roster from the writer.
/// Missing, malformed, duplicated, guest, or foreign-channel entries deny.
pub fn channel_member(
    roster: &Event,
    relay: PublicKey,
    channel: uuid::Uuid,
    key: PublicKey,
) -> bool {
    if u32::from(roster.kind.as_u16()) != crate::kind::KIND_NIP29_GROUP_MEMBERS
        || roster.pubkey != relay
        || roster.verify().is_err()
        || single(roster, "d").ok().flatten() != Some(channel.to_string().as_str())
    {
        return false;
    }
    let key = key.to_hex();
    let entries: Vec<_> = roster
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|tag| tag.first().map(String::as_str) == Some("p") && tag.get(1) == Some(&key))
        .collect();
    match entries.as_slice() {
        [entry] => entry
            .get(3)
            .and_then(|role| role.parse::<buzz_core::channel::MemberRole>().ok())
            .is_some_and(|role| role.can_post()),
        _ => false,
    }
}

fn lifetime_ok(created_at: u64, expires_at: u64) -> bool {
    expires_at > created_at && expires_at - created_at <= MAX_DELEGATION_SECS
}

fn first_d_tag(event: &Event) -> Option<&str> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [name, value, ..] if name == "d" => Some(value.as_str()),
        _ => None,
    })
}

fn single<'a>(event: &'a Event, name: &str) -> Result<Option<&'a str>, SdkError> {
    let mut tags = event
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|tag| tag.first().map(String::as_str) == Some(name));
    let first = tags.next();
    if tags.next().is_some() || first.is_some_and(|tag| tag.len() != 2) {
        return Err(invalid("ambiguous delegation tag"));
    }
    Ok(first.map(|tag| tag[1].as_str()))
}

fn canonical_u64(text: &str) -> Result<u64, SdkError> {
    let value: u64 = text
        .parse()
        .map_err(|_| invalid("invalid delegation expiration"))?;
    if value.to_string() != text {
        return Err(invalid("invalid delegation expiration"));
    }
    Ok(value)
}

fn invalid(message: &str) -> SdkError {
    SdkError::InvalidInput(message.into())
}

#[cfg(test)]
mod tests;
