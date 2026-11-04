//! Sharing uses the session's verified reads and durable outbox, never a
//! private key export or a local preference presented as relay permission.

#[cfg(test)]
#[path = "agent_sharing_tests.rs"]
mod tests;

use super::Session;
use super::actions::{Context, Ops, read};
use super::util::now;
use crate::agent_sharing::{
    ChannelAgent, ChannelAgents, Grant, PendingGrant, owner, recovery_key, remote_agents,
};
use crate::event::{Verified, is_newer, kinds};
use crate::lifecycle::{self, CONFIRM_DELAYS, ChannelState, StateType};
use crate::messages::Draft;
use crate::outbox::{Delivery, Operation, Recovery, SendOptions};
use crate::socket::filter;
use nostr::prelude::EventId;
use serde_json::json;
use std::collections::BTreeMap;

const LIMIT: usize = 500;
const PENDING: &str =
    "The sharing change is saved but not confirmed. Refresh its status or retry the saved change.";

impl Session {
    /// Fresh, relay-authoritative sharing for the viewer's remote agents.
    pub async fn channel_agent_sharing(&self, channel: &str) -> Result<ChannelAgents, String> {
        let (ops, channel) = (self.ops(), channel.to_owned());
        self.run(async move {
            let ctx = ops.context().await?;
            let result = load(&ctx, &channel).await?;
            check_epoch(&ops, ctx.epoch).await?;
            Ok(result)
        })
        .await
    }

    /// One owner action publishes one signed, journaled permission event.
    pub async fn set_channel_agent_sharing(
        &self,
        channel: &str,
        agent: &str,
        enabled: bool,
    ) -> Result<(), String> {
        let (ops, channel, agent) = (self.ops(), channel.to_owned(), agent.to_owned());
        self.run(async move {
            let ctx = ops.context().await?;
            let Some(change) = prepare(&ctx, &channel, &agent, enabled).await? else {
                return Ok(());
            };
            check_epoch(&ops, ctx.epoch).await?;
            let id = ctx
                .outbox
                .send_after(change.draft, change.options, change.after)
                .map_err(|e| e.to_string())?;
            confirm(&ctx, &channel, &agent, id).await
        })
        .await
    }

    /// Retry the exact saved event, after rechecking its current authority.
    pub async fn retry_channel_agent_sharing(&self, id: EventId) -> Result<(), String> {
        let ops = self.ops();
        self.run(async move {
            let ctx = ops.context().await?;
            let op = ctx
                .outbox
                .get(&id)
                .ok_or("The sharing change is no longer pending")?;
            let grant: Grant = serde_json::from_str(&op.event.content)
                .map_err(|_| "Invalid saved sharing change")?;
            if op.kind() != kinds::AGENT_CHANNEL_GRANT || op.event.pubkey.to_hex() != ctx.viewer {
                return Err("Invalid saved sharing change".into());
            }
            let channel = grant.channel.to_string();
            preflight(&ctx, &channel, &grant.agent, grant.enabled).await?;
            check_epoch(&ops, ctx.epoch).await?;
            ctx.outbox.retry(&id, None).map_err(|e| e.to_string())?;
            confirm(&ctx, &channel, &grant.agent, id).await
        })
        .await
    }

    /// Discard only a sharing event proven not to have been delivered.
    pub async fn discard_failed_agent_sharing(&self, id: EventId) -> Result<(), String> {
        let ops = self.ops();
        self.run(async move {
            let ctx = ops.context().await?;
            if let Some(op) = ctx.outbox.get(&id) {
                if op.kind() != kinds::AGENT_CHANNEL_GRANT || op.delivery != Delivery::Failed {
                    return Err(PENDING.into());
                }
                ctx.outbox.dismiss(&id).await.map_err(|e| e.to_string())?;
            }
            Ok(())
        })
        .await
    }
}

struct Change {
    draft: Draft,
    options: SendOptions,
    after: u64,
}

fn pending_for(op: &Operation, channel: &str, agent: &str) -> bool {
    op.kind() == kinds::AGENT_CHANNEL_GRANT
        && op.tag("h") == Some(channel)
        && op.tag("p") == Some(agent)
        && op.recovery.is_some()
}

async fn prepare(
    ctx: &Context,
    channel: &str,
    agent: &str,
    enabled: bool,
) -> Result<Option<Change>, String> {
    ctx.outbox.ready().await.map_err(|e| e.to_string())?;
    preflight(ctx, channel, agent, enabled).await?;
    let heads = grants(ctx, channel).await?;
    settle(ctx, &heads).await?;
    let head = heads.get(agent);
    let pending = ctx.outbox.pending();
    let pending = pending
        .iter()
        .filter(|op| pending_for(op, channel, agent))
        .collect::<Vec<_>>();
    if !pending.is_empty() {
        // An uncertain enable may be cancelled, but never replaced with a new
        // enable. A pending revoke must be reconciled or retried byte-for-byte.
        if enabled || pending.iter().any(|op| !pending_grant(op).enabled) {
            return Err(PENDING.into());
        }
    } else if head
        .map(Grant::parse)
        .transpose()?
        .is_some_and(|g| g.enabled == enabled)
    {
        return Ok(None);
    }
    let after = pending
        .iter()
        .map(|op| op.created_at())
        .chain(head.map(Verified::created_at_secs))
        .max()
        .unwrap_or(0);
    let key = recovery_key(channel, agent);
    Ok(Some(Change {
        draft: Grant::draft(agent, channel, enabled)?,
        options: SendOptions {
            // Keep the old journal until a strictly newer signed revoke is
            // observed. The separate key permits this one cancellation write.
            recovery: Some(Recovery {
                key: if enabled {
                    key
                } else {
                    format!("{key}:revoke")
                },
                value: enabled.to_string(),
            }),
            ..SendOptions::default()
        },
        after,
    }))
}

async fn check_epoch(ops: &Ops, epoch: u64) -> Result<(), String> {
    if ops.with(move |actor| actor.epoch == epoch).await? {
        Ok(())
    } else {
        Err("Community access changed. Refresh sharing settings.".into())
    }
}

async fn state(ctx: &Context, channel: &str) -> Result<ChannelState, String> {
    lifecycle::channel_id(channel).map_err(|e| e.to_string())?;
    ChannelState::parse(
        &read(ctx, lifecycle::state_filters(&ctx.relay, channel)).await?,
        channel,
        &ctx.relay,
    )
    .map_err(|e| e.to_string())
}

async fn listings(ctx: &Context) -> Result<Vec<Verified>, String> {
    let events=read(ctx,vec![filter(json!({"kinds":[kinds::MANAGED_AGENT],"authors":[ctx.viewer],"consistency":"strong","limit":LIMIT}))]).await?;
    if events.len() >= LIMIT {
        return Err("Too many agent records to verify sharing. Contact support.".into());
    }
    Ok(events)
}

async fn grants(ctx: &Context, channel: &str) -> Result<BTreeMap<String, Verified>, String> {
    let events=read(ctx,vec![filter(json!({"kinds":[kinds::AGENT_CHANNEL_GRANT],"authors":[ctx.viewer],"#h":[channel],"consistency":"strong","limit":LIMIT}))]).await?;
    if events.len() >= LIMIT {
        return Err("Too many sharing records to verify safely.".into());
    }
    let mut heads: BTreeMap<String, Verified> = BTreeMap::new();
    for event in events {
        if event.pubkey_hex() != ctx.viewer {
            return Err("Unexpected sharing author".into());
        }
        let grant = Grant::parse(&event)?;
        if grant.channel.to_string() != channel {
            return Err("Unexpected sharing channel".into());
        }
        if heads
            .get(&grant.agent)
            .is_none_or(|old| is_newer(&event, old))
        {
            heads.insert(grant.agent, event);
        }
    }
    Ok(heads)
}

async fn profile(ctx: &Context, agent: &str) -> Result<Verified, String> {
    let events = read(
        ctx,
        vec![filter(
            json!({"kinds":[kinds::PROFILE],"authors":[agent],"consistency":"strong","limit":1}),
        )],
    )
    .await?;
    let event = crate::account::newest_profile(&events, agent)
        .ok_or("The agent's current owner could not be verified")?;
    if owner(event, now()).as_deref() != Some(&ctx.viewer) {
        return Err("Only this agent's current owner can change sharing".into());
    }
    Ok(event.clone())
}

async fn preflight(ctx: &Context, channel: &str, agent: &str, enabled: bool) -> Result<(), String> {
    Grant::draft(agent, channel, enabled)?;
    profile(ctx, agent).await?;
    if enabled {
        let current = state(ctx, channel).await?;
        if current.channel_type == StateType::Dm
            || current.archived
            || !current.roles.contains_key(&ctx.viewer)
            || !current.roles.contains_key(agent)
        {
            return Err(
                "Sharing needs an active channel with both you and the agent as members.".into(),
            );
        }
        if !remote_agents(&listings(ctx).await?, &ctx.viewer).contains(agent) {
            return Err("Move this agent to Remote workspace before sharing it.".into());
        }
    }
    Ok(())
}

async fn settle(ctx: &Context, heads: &BTreeMap<String, Verified>) -> Result<(), String> {
    ctx.outbox.ready().await.map_err(|e| e.to_string())?;
    for op in ctx
        .outbox
        .pending()
        .iter()
        .filter(|op| op.kind() == kinds::AGENT_CHANNEL_GRANT && op.recovery.is_some())
    {
        let Some(agent) = op.tag("p") else {
            continue;
        };
        if let Some(head) = heads.get(agent) {
            if head.id == op.id() {
                ctx.outbox.observe(std::slice::from_ref(head));
                ctx.outbox
                    .acknowledge(&op.id())
                    .await
                    .map_err(|e| e.to_string())?;
            } else if head.tag("d") == op.tag("d")
                && (head.created_at_secs() > op.created_at()
                    || (head.created_at_secs() == op.created_at() && head.id < op.id()))
            {
                ctx.outbox
                    .supersede_agent_grant(&op.id(), head)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

async fn load(ctx: &Context, channel: &str) -> Result<ChannelAgents, String> {
    let current = state(ctx, channel).await?;
    if current.channel_type == StateType::Dm {
        return Err("Direct messages do not share agents.".into());
    }
    let heads = grants(ctx, channel).await?;
    settle(ctx, &heads).await?;
    let remote = remote_agents(&listings(ctx).await?, &ctx.viewer);
    let pending = ctx.outbox.pending();
    let mut keys = remote
        .iter()
        .chain(heads.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    // A runtime change must not hide the only way to reconcile a saved change.
    keys.extend(pending.iter().filter_map(|op| {
        (op.kind() == kinds::AGENT_CHANNEL_GRANT && op.tag("h") == Some(channel))
            .then(|| op.tag("p").map(str::to_owned))
            .flatten()
    }));
    if keys.len() > 128 {
        return Err("Too many agents to verify in one settings panel.".into());
    }
    let profiles = if keys.is_empty() {
        Vec::new()
    } else {
        read(ctx, vec![filter(json!({"kinds":[kinds::PROFILE],"authors":keys,"consistency":"strong","limit":LIMIT}))]).await?
    };
    let mut agents = Vec::new();
    for agent in keys {
        let Some(event) = crate::account::newest_profile(&profiles, &agent) else {
            continue;
        };
        if owner(event, now()).as_deref() != Some(&ctx.viewer) {
            continue;
        }
        let shown = crate::profiles::Profile::from_event(event);
        let enabled = heads
            .get(&agent)
            .map(Grant::parse)
            .transpose()?
            .is_some_and(|g| g.enabled);
        let enable_reason = if current.archived {
            Some("This channel is archived.".into())
        } else if !current.roles.contains_key(&ctx.viewer) || !current.roles.contains_key(&agent) {
            Some("Add yourself and this agent in Members before sharing.".into())
        } else if !remote.contains(&agent) {
            Some("Move this agent to Remote workspace before sharing.".into())
        } else {
            None
        };
        let op = pending
            .iter()
            .filter(|op| pending_for(op, channel, &agent))
            .max_by_key(|op| op.created_at());
        agents.push(ChannelAgent {
            pubkey: agent,
            name: shown.name,
            enabled,
            enable_reason,
            pending: op.map(|op| pending_grant(op)),
        });
    }
    agents.sort_by(|a, b| a.name.cmp(&b.name).then(a.pubkey.cmp(&b.pubkey)));
    Ok(ChannelAgents { agents })
}

fn pending_grant(op: &Operation) -> PendingGrant {
    PendingGrant {
        id: op.id(),
        enabled: serde_json::from_str::<Grant>(&op.event.content).is_ok_and(|g| g.enabled),
        error: op.error.clone(),
        failed: op.delivery == Delivery::Failed,
    }
}

async fn confirm(ctx: &Context, channel: &str, agent: &str, id: EventId) -> Result<(), String> {
    let mut changes = ctx.outbox.changes();
    loop {
        match ctx.outbox.get(&id).map(|op| op.delivery) {
            Some(Delivery::Accepted | Delivery::Seen) => break,
            Some(Delivery::Failed) => {
                return Err(ctx
                    .outbox
                    .get(&id)
                    .and_then(|op| op.error.clone())
                    .unwrap_or_else(|| "Sharing was refused.".into()));
            }
            Some(Delivery::Unknown) | None => return Err(PENDING.into()),
            Some(Delivery::Sending) => changes.changed().await.map_err(|_| PENDING.to_owned())?,
        }
    }
    for delay in CONFIRM_DELAYS {
        tokio::time::sleep(delay).await;
        let heads = grants(ctx, channel).await?;
        settle(ctx, &heads).await?;
        if heads.get(agent).is_some_and(|head| head.id == id) {
            return Ok(());
        }
    }
    Err(PENDING.into())
}
