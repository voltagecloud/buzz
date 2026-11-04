//! Owner opt-in for shared channel agents (kind 30180).

#[cfg(test)]
mod tests;

use crate::{Verified, event::kinds, messages::Draft, profiles};
use nostr::prelude::EventId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One agent the viewer may share from channel settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelAgent {
    /// Agent identity whose owner attestation was verified in this read.
    pub pubkey: String,
    /// The agent's current profile name.
    pub name: String,
    /// The confirmed owner permission, rather than an optimistic local value.
    pub enabled: bool,
    /// Why sharing cannot be enabled; does not block revocation.
    pub enable_reason: Option<String>,
    /// A journaled operation that still needs delivery or confirmation.
    pub pending: Option<PendingGrant>,
}

/// Durable delivery state retained when the settings panel closes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingGrant {
    /// The exact journaled event, reused on retry.
    pub id: EventId,
    /// The requested permission value.
    pub enabled: bool,
    /// Delivery failure or uncertainty retained by the outbox.
    pub error: Option<String>,
    /// True only for proven non-delivery, which permits discarding this event.
    pub failed: bool,
}

/// A verified, fresh channel settings read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelAgents {
    /// Verified owned remote agents and owned agents with existing grants.
    pub agents: Vec<ChannelAgent>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Grant {
    pub version: u32,
    pub agent: String,
    pub channel: Uuid,
    pub enabled: bool,
}

impl Grant {
    pub(crate) fn parse(event: &Verified) -> Result<Self, String> {
        let grant: Self =
            serde_json::from_str(&event.content).map_err(|_| "Invalid sharing permission")?;
        if event.kind_u16() != kinds::AGENT_CHANNEL_GRANT
            || grant.version != 1
            || !crate::event::is_hex64(&grant.agent)
            || grant.agent == event.pubkey_hex()
        {
            return Err("Invalid sharing permission".into());
        }
        for (name, expected) in [
            ("d", format!("{}:{}", grant.agent, grant.channel)),
            ("h", grant.channel.to_string()),
            ("p", grant.agent.clone()),
        ] {
            let tags = event.tags_named(name).collect::<Vec<_>>();
            if tags.len() != 1 || tags[0] != [name, &expected] {
                return Err("Ambiguous sharing permission".into());
            }
        }
        Ok(grant)
    }

    pub(crate) fn draft(agent: &str, channel: &str, enabled: bool) -> Result<Draft, String> {
        let channel = Uuid::parse_str(channel).map_err(|_| "Invalid channel")?;
        if !crate::event::is_hex64(agent) {
            return Err("Invalid agent".into());
        }
        let grant = Self {
            version: 1,
            agent: agent.into(),
            channel,
            enabled,
        };
        Ok(Draft {
            kind: kinds::AGENT_CHANNEL_GRANT,
            tags: vec![
                vec!["d".into(), format!("{agent}:{channel}")],
                vec!["h".into(), channel.to_string()],
                vec!["p".into(), agent.into()],
            ],
            content: serde_json::to_string(&grant).map_err(|error| error.to_string())?,
        })
    }
}

/// Match the relay's current NIP-OA authentication-time ownership check.
pub(crate) fn owner(profile: &Verified, now: u64) -> Option<String> {
    if profile.kind_u16() != kinds::PROFILE {
        return None;
    }
    let tags = profile
        .tags_named("auth")
        .map(<[String]>::to_vec)
        .collect::<Vec<_>>();
    profiles::attested_owner_of(&profile.pubkey_hex(), 22242, now, &tags)
}

pub(crate) fn recovery_key(channel: &str, agent: &str) -> String {
    format!("agent-share:{channel}:{agent}")
}

/// Current runtime selection, never a filtered query that can expose an old head.
pub(crate) fn remote_listing(event: &Verified, viewer: &str) -> Option<String> {
    if event.kind_u16() != kinds::MANAGED_AGENT || event.pubkey_hex() != viewer {
        return None;
    }
    let ids = event.tags_named("d").collect::<Vec<_>>();
    let runtimes = event.tags_named("runtime").collect::<Vec<_>>();
    if ids.len() != 1
        || ids[0].len() != 2
        || !crate::event::is_hex64(&ids[0][1])
        || runtimes.len() != 1
        || runtimes[0] != ["runtime", "voltage.remote-workspace/sandbox"]
    {
        return None;
    }
    Some(ids[0][1].clone())
}

pub(crate) fn remote_agents(
    events: &[Verified],
    viewer: &str,
) -> std::collections::BTreeSet<String> {
    let mut heads: std::collections::BTreeMap<&str, &Verified> = std::collections::BTreeMap::new();
    for event in events
        .iter()
        .filter(|e| e.kind_u16() == kinds::MANAGED_AGENT && e.pubkey_hex() == viewer)
    {
        let Some(id) = event.tag("d") else {
            continue;
        };
        if heads
            .get(id)
            .is_none_or(|old| crate::event::is_newer(event, old))
        {
            heads.insert(id, event);
        }
    }
    heads
        .values()
        .filter_map(|event| remote_listing(event, viewer))
        .collect()
}
