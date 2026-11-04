//! One fresh authorization policy for sleeping-agent startup and running turns.
//! Callers impose an overall deadline and query using the addressed agent's
//! existing authenticated relay identity. No successful verdict is cached.

use super::{
    profile_owner,
    task::{self, Origin, Scope},
    Grant,
};
use nostr::{Event, EventId, Kind, PublicKey};
use serde_json::{json, Value};
use std::future::Future;

/// A permanent policy refusal or an authorization lookup requiring retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// No task may run for this request under the current policy.
    #[error("shared task refused: {0}")]
    Denied(&'static str),
    /// Current authorization could not be established. Retain the task for retry.
    #[error("shared task authorization unavailable")]
    Unavailable,
}

/// Verified provenance to persist in the manager's atomic admission journal.
#[derive(Debug, Clone, Copy)]
pub struct Task {
    /// Immutable conversation scope.
    pub scope: Scope,
    /// The actual signed requester, never replaced with the authorizing owner.
    pub requester: PublicKey,
    /// Addressed agent.
    pub agent: PublicKey,
    /// Target agent's current owner.
    pub owner: PublicKey,
    /// Original human request and preceding admission.
    pub origin: Origin,
    /// Original human identity.
    pub human: PublicKey,
    /// Initial agent's owner; chooses its existing namespace for the journal.
    pub origin_owner: PublicKey,
    /// The incoming task event.
    pub trigger: EventId,
    /// Whether the requester is a verified agent.
    pub handoff: bool,
    /// Signed creation time of the original human task. Expiry cannot reset.
    pub requested_at: u64,
}

/// Load current grants, identities, metadata and membership, then verify task
/// provenance. A caller must atomically admit the returned task before execution.
pub async fn authorize<F, Fut>(
    mut query: F,
    relay: PublicKey,
    owner: PublicKey,
    agent: PublicKey,
    event: &Event,
    now: u64,
) -> Result<Task, Error>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = Result<Vec<Event>, ()>>,
{
    let scope = task::scope(event, agent, now).map_err(|_| Error::Denied("invalid mention"))?;
    let stored = one(
        &mut query,
        json!({"kinds":[9,40002], "ids":[event.id], "#h":[scope.channel], "limit":1}),
    )
    .await?
    .ok_or(Error::Denied("task is not currently visible on the relay"))?;
    if stored != *event {
        return Err(Error::Denied("task does not match the signed relay event"));
    }
    let roster = one(
        &mut query,
        json!({"kinds":[39002], "authors":[relay], "#d":[scope.channel], "limit":1}),
    )
    .await?
    .ok_or(Error::Unavailable)?;
    let metadata = one(
        &mut query,
        json!({"kinds":[39000], "authors":[relay], "#d":[scope.channel], "limit":1}),
    )
    .await?
    .ok_or(Error::Unavailable)?;
    for (snapshot, kind) in [(&roster, 39002), (&metadata, 39000)] {
        if snapshot.pubkey != relay
            || snapshot.kind.as_u16() != kind
            || snapshot.verify().is_err()
            || task::single_tag(snapshot, "d", 2)
                .map_err(|_| Error::Unavailable)?
                .is_none_or(|t| t[1] != scope.channel.to_string())
        {
            return Err(Error::Unavailable);
        }
    }
    if metadata.tags.iter().any(|t| {
        let t = t.as_slice();
        t == ["t", "dm"]
            || t == ["archived", "true"]
            || t.first().map(String::as_str) == Some("hidden")
    }) {
        return Err(Error::Denied("conversation cannot be shared"));
    }
    for member in [owner, agent, event.pubkey] {
        member_role(&roster, member)?;
    }
    let target = profile(&mut query, agent).await?;
    if profile_owner(&target, now).ok() != Some(owner) {
        return Err(Error::Denied("agent ownership changed"));
    }
    grant(&mut query, owner, agent, &scope, event, now).await?;
    // Channel roles and self-authored profiles can change. Only a direct
    // workspace member may originate a human budget; an agent cannot gain a
    // fresh budget by stripping its profile credential or rejoining as member.
    let humans = one(
        &mut query,
        json!({"kinds":[crate::kind::KIND_NIP43_MEMBERSHIP_LIST], "authors":[relay], "limit":1}),
    )
    .await?
    .ok_or(Error::Unavailable)?;
    if humans.pubkey != relay
        || humans.kind.as_u16() as u32 != crate::kind::KIND_NIP43_MEMBERSHIP_LIST
        || humans.verify().is_err()
    {
        return Err(Error::Unavailable);
    }
    let requester = profile(&mut query, event.pubkey).await?;
    let automated = is_agent(&requester, member_role(&roster, event.pubkey)?, now)?;
    let claimed = task::origin(event).map_err(|_| Error::Denied("invalid task ancestry"))?;
    let origin = if automated {
        claimed.ok_or(Error::Denied("agent handoff requires task ancestry"))?
    } else {
        workspace_human(&humans, event.pubkey)?;
        if claimed.is_some() {
            return Err(Error::Denied("human tasks cannot claim agent ancestry"));
        }
        Origin {
            request: event.id,
            agent,
            parent: event.id,
        }
    };
    let root = if origin.request == event.id {
        event.clone()
    } else {
        one(
            &mut query,
            json!({"kinds":[9,40002], "ids":[origin.request], "#h":[scope.channel], "limit":1}),
        )
        .await?
        .ok_or(Error::Unavailable)?
    };
    if root.id != origin.request
        || task::scope(&root, origin.agent, now).ok() != Some(scope)
        || task::origin(&root)
            .map_err(|_| Error::Denied("invalid human request"))?
            .is_some()
    {
        return Err(Error::Denied("request is outside this task"));
    }
    let human = profile(&mut query, root.pubkey).await?;
    if is_agent(&human, member_role(&roster, root.pubkey)?, now)? {
        return Err(Error::Denied("an agent cannot reset the human budget"));
    }
    workspace_human(&humans, root.pubkey)?;
    if automated {
        ancestry(&mut query, event, &root, origin, scope, now).await?;
    }
    member_role(&roster, origin.agent)?;
    let initial = profile(&mut query, origin.agent).await?;
    let origin_owner =
        profile_owner(&initial, now).map_err(|_| Error::Denied("invalid initial agent owner"))?;
    member_role(&roster, origin_owner)?;
    if root.pubkey != origin_owner {
        grant(&mut query, origin_owner, origin.agent, &scope, &root, now).await?;
    }
    Ok(Task {
        scope,
        requester: event.pubkey,
        agent,
        owner,
        origin,
        human: root.pubkey,
        origin_owner,
        trigger: event.id,
        handoff: automated,
        requested_at: root.created_at.as_secs(),
    })
}

// Verify the signed relationship as well as the later manager's durable
// receipt. Four links bound both work and relay queries; a cycle never grows.
async fn ancestry<F, Fut>(
    query: &mut F,
    event: &Event,
    root: &Event,
    origin: Origin,
    scope: Scope,
    now: u64,
) -> Result<(), Error>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = Result<Vec<Event>, ()>>,
{
    let denied = || Error::Denied("handoff parent does not match this task");
    let mut child = event.clone();
    for _ in 0..4 {
        let claimed = task::origin(&child)
            .map_err(|_| denied())?
            .ok_or_else(denied)?;
        if claimed.request != origin.request || claimed.agent != origin.agent {
            return Err(denied());
        }
        let replies: Vec<_> = child
            .tags
            .iter()
            .map(nostr::Tag::as_slice)
            .filter(|t| {
                t.first().map(String::as_str) == Some("e")
                    && t.get(3).map(String::as_str) == Some("reply")
            })
            .collect();
        if replies.len() != 1 || replies[0].get(1) != Some(&claimed.parent.to_hex()) {
            return Err(denied());
        }
        let parent = if claimed.parent == root.id {
            root.clone()
        } else {
            one(
                query,
                json!({"kinds":[9,40002],"ids":[claimed.parent],"#h":[scope.channel],"limit":1}),
            )
            .await?
            .ok_or_else(denied)?
        };
        if parent.id != claimed.parent
            || task::scope(&parent, child.pubkey, now).ok() != Some(scope)
        {
            return Err(denied());
        }
        if parent.id == root.id {
            return Ok(());
        }
        child = parent;
    }
    Err(Error::Denied("handoff ancestry exceeds the work budget"))
}

fn workspace_human(members: &Event, key: PublicKey) -> Result<(), Error> {
    let key = key.to_hex();
    if members.tags.iter().any(|t| {
        let t = t.as_slice();
        t.len() == 3
            && t[0] == "member"
            && t[1] == key
            && matches!(t[2].as_str(), "owner" | "admin" | "member")
    }) {
        Ok(())
    } else {
        Err(Error::Denied(
            "human task requires a direct workspace member",
        ))
    }
}

fn member_role(roster: &Event, key: PublicKey) -> Result<&str, Error> {
    let key = key.to_hex();
    let tags: Vec<_> = roster
        .tags
        .iter()
        .map(|t| t.as_slice())
        .filter(|t| t.first().map(String::as_str) == Some("p") && t.get(1) == Some(&key))
        .collect();
    if tags.len() != 1 {
        return Err(Error::Denied(
            "requester, owner and agent must remain channel members",
        ));
    }
    tags[0].get(3).map(String::as_str).ok_or(Error::Unavailable)
}

fn is_agent(profile: &Event, role: &str, now: u64) -> Result<bool, Error> {
    let has_auth = profile
        .tags
        .iter()
        .any(|t| t.as_slice().first().map(String::as_str) == Some("auth"));
    if has_auth {
        profile_owner(profile, now).map_err(|_| Error::Denied("invalid requester identity"))?;
        return Ok(true);
    }
    if role == "bot" {
        return Err(Error::Denied("bot cannot claim human identity"));
    }
    Ok(false)
}

async fn profile<F, Fut>(query: &mut F, key: PublicKey) -> Result<Event, Error>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = Result<Vec<Event>, ()>>,
{
    let event = one(query, json!({"kinds":[0],"authors":[key],"limit":1}))
        .await?
        .ok_or(Error::Unavailable)?;
    if event.pubkey != key || event.kind != Kind::Metadata || event.verify().is_err() {
        return Err(Error::Unavailable);
    }
    Ok(event)
}

async fn grant<F, Fut>(
    query: &mut F,
    owner: PublicKey,
    agent: PublicKey,
    scope: &Scope,
    event: &Event,
    now: u64,
) -> Result<(), Error>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = Result<Vec<Event>, ()>>,
{
    let head = one(
        query,
        json!({"kinds":[crate::kind::KIND_AGENT_CHANNEL_GRANT],"authors":[owner],
        "#h":[scope.channel],"#d":[Grant::coordinate(agent, scope.channel)],"limit":1}),
    )
    .await?
    .ok_or(Error::Denied("agent is private in this channel"))?;
    let grant = Grant::parse(&head).map_err(|_| Error::Denied("invalid sharing grant"))?;
    if head.pubkey != owner
        || grant.agent != agent
        || grant.channel != scope.channel
        || !grant.enabled
        || head.created_at.as_secs() > now.saturating_add(60)
        || event.created_at < head.created_at
    {
        return Err(Error::Denied(
            "sharing grant is revoked or does not cover this task",
        ));
    }
    Ok(())
}

async fn one<F, Fut>(query: &mut F, filter: Value) -> Result<Option<Event>, Error>
where
    F: FnMut(Value) -> Fut,
    Fut: Future<Output = Result<Vec<Event>, ()>>,
{
    let mut events = query(filter).await.map_err(|_| Error::Unavailable)?;
    if events.len() > 1 {
        return Err(Error::Unavailable);
    }
    Ok(events.pop())
}

#[cfg(test)]
mod tests;
