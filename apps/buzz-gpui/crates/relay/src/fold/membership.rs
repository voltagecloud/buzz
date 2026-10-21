//! Membership activity rows: relay-signed kind 40099 (`membership.ts`,
//! §5g.4).

use super::{object_body, pubkey};
use crate::event::kinds;
use nostr::prelude::{Event, PublicKey};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MembershipKind {
    Joined,
    Left,
    Removed,
}

/// Relay-authored activity, never a membership grant or a user message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MembershipChange {
    pub kind: MembershipKind,
    pub actor: PublicKey,
    pub target: PublicKey,
}

/// Only the relay's own key may describe membership activity. Other 40099
/// types (channel created, topic changed, ...) are not membership rows.
pub fn membership_change(event: &Event, relay_author: &PublicKey) -> Option<MembershipChange> {
    if event.kind.as_u16() != kinds::SYSTEM_MESSAGE || event.pubkey != *relay_author {
        return None;
    }
    let body = object_body(&event.content)?;
    let key = |name: &str| body.get(name).and_then(Value::as_str).and_then(pubkey);
    let actor = key("actor")?;
    let (kind, target) = match body.get("type").and_then(Value::as_str)? {
        "member_left" => {
            // A leave names nobody else.
            match body.get("target") {
                None => {}
                Some(target) if target.as_str() == body.get("actor").and_then(Value::as_str) => {}
                Some(_) => return None,
            }
            (MembershipKind::Left, actor)
        }
        "member_joined" => (MembershipKind::Joined, key("target")?),
        "member_removed" => (MembershipKind::Removed, key("target")?),
        _ => return None,
    };
    Some(MembershipChange {
        kind,
        actor,
        target,
    })
}
