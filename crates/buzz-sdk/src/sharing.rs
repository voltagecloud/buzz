//! Owner-authored permission to invoke one agent in one channel.
//!
//! Grants convey task invocation only. They never authorize lifecycle operations,
//! credential access, or access to other conversations. Consumers must also check
//! current channel membership before accepting a task.

use nostr::{Event, EventBuilder, Kind, PublicKey, Tag};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{kind::KIND_AGENT_CHANNEL_GRANT, nip_oa, SdkError};

pub mod policy;
pub mod task;

/// One versioned owner opt-in. A disabled head is a durable revocation.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// Wire version; currently one.
    pub version: u32,
    /// The agent whose configured tools members may invoke.
    pub agent: PublicKey,
    /// The single conversation in which invocation is permitted.
    pub channel: Uuid,
    /// Whether members may send explicit task mentions.
    pub enabled: bool,
}

impl Grant {
    /// The stable NIP-33 coordinate, including both target and channel.
    pub fn coordinate(agent: PublicKey, channel: Uuid) -> String {
        format!("{agent}:{channel}")
    }

    /// Build a public grant without embedding runtime configuration or secrets.
    /// The caller must sign with the human owner's key.
    pub fn builder(
        agent: PublicKey,
        channel: Uuid,
        enabled: bool,
    ) -> Result<EventBuilder, SdkError> {
        let grant = Self {
            version: 1,
            agent,
            channel,
            enabled,
        };
        let tags = [
            vec!["d".into(), Self::coordinate(agent, channel)],
            vec!["h".into(), channel.to_string()],
            vec!["p".into(), agent.to_hex()],
        ]
        .into_iter()
        .map(Tag::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| SdkError::InvalidTag(e.to_string()))?;
        let content =
            serde_json::to_string(&grant).map_err(|e| SdkError::InvalidInput(e.to_string()))?;
        Ok(EventBuilder::new(Kind::Custom(KIND_AGENT_CHANNEL_GRANT as u16), content).tags(tags))
    }

    /// Verify the signature, version and unambiguous signed scope.
    /// Ownership is checked separately against the agent's live profile.
    pub fn parse(event: &Event) -> Result<Self, SdkError> {
        if event.kind.as_u16() as u32 != KIND_AGENT_CHANNEL_GRANT || event.verify().is_err() {
            return Err(invalid("invalid grant signature or kind"));
        }
        let grant: Self =
            serde_json::from_str(&event.content).map_err(|_| invalid("invalid grant content"))?;
        if grant.version != 1 || grant.agent == event.pubkey {
            return Err(invalid("invalid grant version or agent"));
        }
        for (name, value) in [
            ("d", Self::coordinate(grant.agent, grant.channel)),
            ("h", grant.channel.to_string()),
            ("p", grant.agent.to_hex()),
        ] {
            let tags: Vec<_> = event
                .tags
                .iter()
                .filter(|tag| tag.as_slice().first().map(String::as_str) == Some(name))
                .collect();
            if tags.len() != 1 || tags[0].as_slice() != [name, &value] {
                return Err(invalid("ambiguous grant scope"));
            }
        }
        Ok(grant)
    }
}

/// Verify an agent's current kind:0 owner attestation at admission time.
/// Missing or ambiguous credentials fail closed; caller obtains the current head.
pub fn profile_owner(profile: &Event, now: u64) -> Result<PublicKey, SdkError> {
    if profile.kind != Kind::Metadata || profile.verify().is_err() {
        return Err(invalid("invalid agent profile"));
    }
    let auth: Vec<_> = profile
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("auth"))
        .collect();
    if auth.len() != 1 || auth[0].as_slice().len() != 4 {
        return Err(invalid("agent profile must have one owner attestation"));
    }
    let text = serde_json::to_string(auth[0].as_slice())
        .map_err(|_| invalid("invalid owner attestation"))?;
    nip_oa::verify_auth_tag_for_auth_event(&text, &profile.pubkey, now)
}

fn invalid(message: &str) -> SdkError {
    SdkError::InvalidInput(message.into())
}

#[cfg(test)]
mod tests;
