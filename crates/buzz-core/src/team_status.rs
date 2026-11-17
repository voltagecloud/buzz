//! Owner-published team status envelope. Parsing does not grant publication or read access.
//!
//! Callers must verify signatures, current ownership, membership and root access.
//! Expiry hides a winning head; it must never remove the ordering watermark.

use nostr::{Event, EventId, PublicKey};
use serde::Deserialize;
use uuid::Uuid;

use crate::{artifact::canonical_uuid, kind::KIND_TEAM_AGENT_STATUS, CommunityId};

/// Maximum lifetime of a status, in seconds.
pub const MAX_LIFETIME: u64 = 900;
/// Maximum accepted clock skew into the future, in seconds.
pub const MAX_FUTURE_SKEW: u64 = 60;
/// Maximum encoded content size.
pub const MAX_CONTENT_BYTES: usize = 512;
/// Maximum label length, in Unicode scalar values.
pub const MAX_LABEL_CHARS: usize = 96;
/// Maximum accepted writes per owner-agent-channel per minute.
pub const MAX_WRITES_PER_MINUTE: i64 = 12;
/// Maximum concurrently live thread addresses per owner-agent-channel.
pub const MAX_ACTIVE_THREADS: i64 = 32;

/// A coarse owner assertion, independent of presence or observed telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Owner reports ongoing work.
    Working,
    /// Owner reports waiting.
    Waiting,
    /// Owner reports a blocker.
    Blocked,
    /// Owner reports completion; this is not independent verification.
    Done,
}

impl Status {
    /// Canonical wire label prefix.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

/// Validated content; an empty string clears the address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    /// Remove the visible status, retaining its ordering head.
    Clear,
    /// A coarse status and optional owner-approved public caption.
    Label {
        /// Coarse work state.
        status: Status,
        /// Optional public caption. Its provenance remains the owner's responsibility.
        caption: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    v: u8,
    label: String,
}

/// Parse content without inferring whether an arbitrary caption describes private work.
pub fn parse_content(content: &str) -> Result<Content, &'static str> {
    if content.is_empty() {
        return Ok(Content::Clear);
    }
    if content.len() > MAX_CONTENT_BYTES {
        return Err("team status content exceeds 512 bytes");
    }
    let payload: Payload = serde_json::from_str(content).map_err(|_| "invalid team status JSON")?;
    if payload.v != 1 {
        return Err("unsupported team status version");
    }
    let label = payload.label;
    if label.chars().count() > MAX_LABEL_CHARS
        || label
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        || label.contains(['@', '/', '\\', '`'])
        || label.to_ascii_lowercase().contains("nostr:")
    {
        return Err("invalid team status label");
    }
    let (prefix, caption) = match label.split_once(": ") {
        Some((prefix, caption)) if !caption.is_empty() && caption.trim() == caption => {
            (prefix, Some(caption.to_owned()))
        }
        Some(_) => return Err("invalid team status caption"),
        None => (label.as_str(), None),
    };
    let status = match prefix {
        "working" => Status::Working,
        "waiting" => Status::Waiting,
        "blocked" => Status::Blocked,
        "done" => Status::Done,
        _ => return Err("invalid team status state"),
    };
    Ok(Content::Label { status, caption })
}

/// Parsed routing fields and payload. This is structural validation, not authorization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    /// Subject agent, never a notification recipient.
    pub agent: PublicKey,
    /// Publication audience and root's channel.
    pub channel: Uuid,
    /// Mandatory channel-local thread root.
    pub root: EventId,
    /// Exact replacement address.
    pub address: String,
    /// Display expiry, in Unix seconds.
    pub expires_at: u64,
    /// Validated content or clear marker.
    pub content: Content,
}

/// Construct the canonical address from a server-resolved community and scope.
pub fn address(community: CommunityId, agent: PublicKey, channel: Uuid, root: EventId) -> String {
    format!(
        "team-status:{community}:{}:{channel}:{}",
        agent.to_hex(),
        root.to_hex()
    )
}

/// Validate a signed envelope's shape and community binding, including expired heads.
///
/// The caller supplies the trusted community. This function deliberately does not
/// reject expired heads: consumers select the winner before hiding clear/expired state.
/// Signature and database authorization checks are the caller's responsibility.
pub fn parse(event: &Event, community: CommunityId) -> Result<Envelope, &'static str> {
    if event.kind.as_u16() as u32 != KIND_TEAM_AGENT_STATUS {
        return Err("not a team status event");
    }
    if event.tags.len() != 6 {
        return Err("team status requires exactly six routing tags");
    }
    let mut fields = std::collections::HashMap::new();
    for tag in event.tags.iter() {
        let parts = tag.as_slice();
        let name = parts.first().ok_or("empty team status tag")?.as_str();
        let expected = if name == "e" { 4 } else { 2 };
        if !["d", "community", "h", "p", "e", "expiration"].contains(&name)
            || parts.len() != expected
            || fields.insert(name, parts).is_some()
        {
            return Err("invalid or duplicate team status tag");
        }
    }
    let field = |name| fields.get(name).copied().ok_or("missing team status tag");
    if canonical_uuid(&field("community")?[1])? != *community.as_uuid() {
        return Err("team status community mismatch");
    }
    let channel = canonical_uuid(&field("h")?[1])?;
    let agent_hex = &field("p")?[1];
    let agent = PublicKey::from_hex(agent_hex).map_err(|_| "invalid team status agent")?;
    if agent.to_hex() != *agent_hex {
        return Err("noncanonical team status agent");
    }
    let root_parts = field("e")?;
    let root = EventId::from_hex(&root_parts[1]).map_err(|_| "invalid team status root")?;
    if root.to_hex() != root_parts[1] || !root_parts[2].is_empty() || root_parts[3] != "root" {
        return Err("invalid team status root tag");
    }
    let address = address(community, agent, channel, root);
    if field("d")?[1] != address {
        return Err("team status address mismatch");
    }
    let expiry = &field("expiration")?[1];
    let expires_at: u64 = expiry.parse().map_err(|_| "invalid team status expiry")?;
    let created = event.created_at.as_secs();
    if expires_at.to_string() != *expiry
        || expires_at <= created
        || expires_at - created > MAX_LIFETIME
    {
        return Err("team status lifetime must be within 15 minutes");
    }
    Ok(Envelope {
        agent,
        channel,
        root,
        address,
        expires_at,
        content: parse_content(&event.content)?,
    })
}

/// Validate publication time separately from parsing stored ordering heads.
pub fn validate_time(event: &Event, envelope: &Envelope, now: u64) -> Result<(), &'static str> {
    if event.created_at.as_secs() > now.saturating_add(MAX_FUTURE_SKEW) {
        return Err("team status creation is too far in the future");
    }
    if envelope.expires_at <= now {
        return Err("team status has expired");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
