//! Signed task scope and explicit, verifiable handoff ancestry.

use crate::SdkError;
use nostr::{Event, EventBuilder, EventId, Kind, PublicKey, Tag};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A mention's signed channel and canonical thread binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// The sole canonical channel tag.
    pub channel: Uuid,
    /// The NIP-10 containing thread, or the message itself for a new thread.
    pub root: EventId,
}

/// Provenance retained across cross-owner agent handoffs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    /// The original human's signed task message.
    pub request: EventId,
    /// The agent that human initially addressed.
    pub agent: PublicKey,
    /// The incoming task the handing-off agent is processing.
    pub parent: EventId,
}

fn invalid() -> SdkError {
    SdkError::InvalidInput("invalid shared task scope or provenance".into())
}

/// Read exactly one named tag, rejecting duplicates and malformed arity.
pub fn single_tag<'a>(
    event: &'a Event,
    name: &str,
    arity: usize,
) -> Result<Option<&'a [String]>, SdkError> {
    let mut tags = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some(name));
    let first = tags.next().map(Tag::as_slice);
    if tags.next().is_some() || first.is_some_and(|tag| tag.len() != arity) {
        return Err(invalid());
    }
    Ok(first)
}

/// Verify an explicit actionable mention and derive its immutable scope.
pub fn scope(event: &Event, agent: PublicKey, now: u64) -> Result<Scope, SdkError> {
    if event.verify().is_err()
        || !matches!(event.kind.as_u16(), 9 | 40002)
        || event.pubkey == agent
        || event.created_at.as_secs() > now.saturating_add(60)
    {
        return Err(invalid());
    }
    let channel = single_tag(event, "h", 2)?.ok_or_else(invalid)?;
    let parsed = Uuid::parse_str(&channel[1]).map_err(|_| invalid())?;
    if parsed.to_string() != channel[1] {
        return Err(invalid());
    }
    let addressed = event
        .tags
        .iter()
        .filter(|tag| {
            let p = tag.as_slice();
            p.first().map(String::as_str) == Some("p")
                && p.get(1).map(String::as_str) == Some(&agent.to_hex())
        })
        .count();
    if addressed != 1 {
        return Err(invalid());
    }
    let mut root = None;
    let mut reply = None;
    for tag in event
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|t| t.first().map(String::as_str) == Some("e"))
    {
        let slot = match tag.get(3).map(String::as_str) {
            Some("root") => &mut root,
            Some("reply") => &mut reply,
            Some("mention") => continue,
            _ => return Err(invalid()),
        };
        let id = EventId::from_hex(tag.get(1).ok_or_else(invalid)?).map_err(|_| invalid())?;
        if id.to_hex() != tag[1] || slot.replace(id).is_some() {
            return Err(invalid());
        }
    }
    Ok(Scope {
        channel: parsed,
        root: root.or(reply).unwrap_or(event.id),
    })
}

/// Decode the paired task/parent tags. Claimed ancestry never grants access by itself.
pub fn origin(event: &Event) -> Result<Option<Origin>, SdkError> {
    let task = single_tag(event, "buzz-task", 3)?;
    let parent = single_tag(event, "buzz-parent", 2)?;
    match (task, parent) {
        (None, None) => Ok(None),
        (Some(task), Some(parent)) => Ok(Some(Origin {
            request: EventId::from_hex(&task[1]).map_err(|_| invalid())?,
            agent: PublicKey::from_hex(&task[2]).map_err(|_| invalid())?,
            parent: EventId::from_hex(&parent[1]).map_err(|_| invalid())?,
        })),
        _ => Err(invalid()),
    }
}

/// Build an explicit handoff from an admitted incoming message. The manager
/// independently checks the parent's durable admission and remaining budget.
pub fn handoff(
    source: &Event,
    sender: PublicKey,
    target: PublicKey,
    text: &str,
    now: u64,
) -> Result<EventBuilder, SdkError> {
    let scope = scope(source, sender, now)?;
    if sender == target || text.trim().is_empty() || text.len() > 60_000 {
        return Err(invalid());
    }
    let prior = origin(source)?;
    let origin = prior.unwrap_or(Origin {
        request: source.id,
        agent: sender,
        parent: source.id,
    });
    let tags = [
        vec!["h".into(), scope.channel.to_string()],
        vec!["p".into(), target.to_hex()],
        vec!["e".into(), scope.root.to_hex(), "".into(), "root".into()],
        vec!["e".into(), source.id.to_hex(), "".into(), "reply".into()],
        vec![
            "buzz-task".into(),
            origin.request.to_hex(),
            origin.agent.to_hex(),
        ],
        vec!["buzz-parent".into(), source.id.to_hex()],
    ]
    .into_iter()
    .map(Tag::parse)
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| invalid())?;
    Ok(EventBuilder::new(Kind::Custom(9), text).tags(tags))
}
