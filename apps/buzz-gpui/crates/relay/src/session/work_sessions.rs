//! Work sessions (`relay/work-sessions.ts`): private streams that hold a
//! conversation with agents, created and joined through the same durable
//! outbox as every other write.
//!
//! Admission never comes from a command's acknowledgment. After a create
//! or an addition, [`Session::work_session_refresh`] waits until the
//! session's channel list carries a relay-signed roster with the expected
//! members, reading that one channel first and the whole roster if that
//! is not enough.

use super::actions::{Context, Ops, read};
use super::delivery::{Wait, delivered as waiting};
use super::{Session, SessionSnapshot};
use crate::discovery::{
    ChannelSummary, ChannelType, SESSION_CHANNEL_DESCRIPTION, ScanOptions, is_uuid as uuid,
};
use crate::event::{Verified, kinds};
use crate::lifecycle;
use crate::members::is_key;
use crate::messages::Draft;
use crate::outbox::{Delivery, SendOptions};
use crate::reader::{Priority, ReadOptions};
use crate::socket::Filter;
use crate::{RosterRefresh, RosterStatus};
use nostr::prelude::EventId;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// How long a delivery or roster confirmation may take.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);
/// An unresolved agent addition this old must be cleared by hand.
const ADD_EXPIRY_SECS: u64 = 15 * 60;
/// `memberChannels` refuses a possibly truncated answer.
const MEMBER_CHANNEL_LIMIT: usize = 500;
const MEMBERSHIP_UNAVAILABLE: &str = "Channel membership is unavailable.";
const REFRESH_MEMBERSHIP: &str = "Could not refresh channel membership. Retry to continue.";

/// The session description, with the parent of a child session
/// (`sessionDescription`).
pub fn session_description(parent: Option<&str>) -> String {
    match parent {
        Some(parent) => format!("{SESSION_CHANNEL_DESCRIPTION}\nparent:{parent}"),
        None => SESSION_CHANNEL_DESCRIPTION.to_owned(),
    }
}

/// The create command of a session (`workSessions.create`).
pub fn create_draft(id: &str, title: &str, parent: Option<&str>) -> Result<Draft, String> {
    if !uuid(id) || parent.is_some_and(|parent| !uuid(parent)) {
        return Err("Invalid session identifier".into());
    }
    if title.trim().is_empty() || title.chars().count() > 120 {
        return Err("Use a session title between 1 and 120 characters.".into());
    }
    if parent == Some(id) {
        return Err("A session cannot be its own parent.".into());
    }
    Ok(Draft {
        kind: kinds::CREATE_CHANNEL,
        content: String::new(),
        tags: vec![
            vec!["h".into(), id.into()],
            vec!["name".into(), title.into()],
            vec!["visibility".into(), "private".into()],
            vec!["channel_type".into(), "stream".into()],
            vec!["about".into(), session_description(parent)],
        ],
    })
}

/// The title a new session gets from its first message: whitespace
/// folded, at most 80 characters.
pub fn session_title(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

fn add_draft(channel: &str, pubkey: &str) -> Draft {
    Draft {
        kind: kinds::PUT_USER,
        content: String::new(),
        tags: vec![
            vec!["h".into(), channel.into()],
            vec!["p".into(), pubkey.into()],
        ],
    }
}

/// What the channel list must show before a write counts as admitted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expected {
    pub member: Option<String>,
    pub members: Vec<String>,
    pub parent: Option<String>,
}

impl Expected {
    pub fn member(pubkey: &str) -> Self {
        Self {
            member: Some(pubkey.into()),
            ..Self::default()
        }
    }

    pub fn members(members: Vec<String>) -> Self {
        Self {
            members,
            ..Self::default()
        }
    }

    pub fn parent(parent: &str) -> Self {
        Self {
            parent: Some(parent.into()),
            ..Self::default()
        }
    }

    fn satisfied(&self, channel: &ChannelSummary, session_only: bool) -> bool {
        (!session_only || channel.channel_type == Some(ChannelType::Session))
            && self
                .member
                .as_ref()
                .is_none_or(|member| channel.members.contains(member))
            && self.members.iter().all(|key| channel.members.contains(key))
            && self
                .parent
                .as_ref()
                .is_none_or(|parent| channel.parent_channel_id.as_ref() == Some(parent))
    }
}

/// Which agents a session may admit (`agents` in `createWorkSessions`).
#[derive(Clone, Debug, Default)]
pub struct AgentRules {
    /// The viewer's selectable agents.
    pub selectable: Vec<String>,
    /// Agents whose identity is archived.
    pub archived: Vec<String>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn strong_ids(id: &str) -> Filter {
    Filter::from_iter([
        ("ids".to_owned(), json!([id])),
        ("limit".to_owned(), json!(1)),
        ("consistency".to_owned(), json!("strong")),
    ])
}

/// Waits for a work-session write to be delivered (`delivered`): retries a
/// failed or uncertain send unless `retry` is off, and gives up after 15
/// seconds. `addition` picks the agent-addition copy.
pub(super) async fn delivered(
    ctx: &Context,
    id: EventId,
    addition: bool,
    retry: bool,
) -> Result<(), String> {
    let hex = id.to_hex();
    let found = |events: &[Verified]| events.iter().any(|event| event.id_hex() == hex);
    let Some(existing) = ctx.outbox.get(&id) else {
        if creation_receipt(ctx, &id).await? || found(&read(ctx, vec![strong_ids(&hex)]).await?) {
            return Ok(());
        }
        return Err("The saved operation could not be confirmed. Reconnect and retry.".into());
    };
    if matches!(existing.delivery, Delivery::Failed | Delivery::Unknown) {
        // A repeated channel UUID is refused; an exact verified creation
        // event confirms an earlier lost acknowledgment instead.
        if existing.kind() == kinds::CREATE_CHANNEL {
            if creation_receipt(ctx, &id).await? {
                return Ok(());
            }
            let channel = existing
                .tag("h")
                .ok_or("The saved channel creation is invalid.")?
                .to_owned();
            let events = read(
                ctx,
                vec![
                    Filter::from_iter([
                        (
                            "kinds".to_owned(),
                            json!([kinds::CHANNEL_METADATA, kinds::CHANNEL_MEMBERS]),
                        ),
                        ("#d".to_owned(), json!([channel])),
                        ("limit".to_owned(), json!(2)),
                        ("consistency".to_owned(), json!("strong")),
                    ]),
                    strong_ids(&hex),
                ],
            )
            .await?;
            let author = existing.event.pubkey.to_hex();
            if events
                .iter()
                .any(|event| event.id_hex() == hex && event.pubkey_hex() == author)
            {
                return Ok(());
            }
        }
        if !retry {
            if found(&read(ctx, vec![strong_ids(&hex)]).await?) {
                return Ok(());
            }
            return Err(existing
                .error
                .clone()
                .unwrap_or_else(|| "The operation could not be confirmed.".into()));
        }
        ctx.outbox
            .retry(&id, None)
            .map_err(|error| error.to_string())?;
    }
    let wait = Wait {
        missing: Some("The operation could not be confirmed."),
        failed: if addition {
            "Agent addition could not be confirmed. Check membership before retrying."
        } else {
            "The operation could not be confirmed."
        },
        unknown_fails: true,
        closed: if addition {
            "The community connection changed. Check channel membership before adding again."
        } else {
            "The community connection changed. Your draft is kept."
        },
    };
    tokio::time::timeout(CONFIRM_TIMEOUT, waiting(&ctx.outbox, id, &wait))
        .await
        .unwrap_or_else(|_| {
            Err(if addition {
                "Agent addition is unconfirmed. Check channel membership before retrying; the request may already have reached the relay.".into()
            } else {
                "Still waiting for confirmation. Retry without starting another session.".into()
            })
        })
}

/// The viewer's exact creation receipt (`confirmCreation`). Never admits
/// the channel.
async fn creation_receipt(ctx: &Context, id: &EventId) -> Result<bool, String> {
    let events = read(
        ctx,
        vec![lifecycle::creation_receipt_filter(id, &ctx.viewer)],
    )
    .await?;
    Ok(events.iter().any(|event| {
        event.id_hex() == id.to_hex()
            && event.kind_u16() == kinds::CREATE_CHANNEL
            && event.pubkey_hex() == ctx.viewer
    }))
}

fn listed(snapshot: &SessionSnapshot, id: &str) -> Option<ChannelSummary> {
    snapshot
        .channels
        .iter()
        .find(|channel| channel.id == id)
        .cloned()
}

/// Tells a roster error from a scan that started after a refresh began
/// apart from an earlier one, which stays until the next scan finishes.
#[derive(Default)]
struct FreshScan {
    /// No scan was running at some point since the refresh began.
    idle: bool,
    /// A scan started after that.
    started: bool,
}

impl FreshScan {
    /// The roster error that decides the refresh, if any.
    fn error(&mut self, refresh: &RosterRefresh, roster: &RosterStatus) -> Option<String> {
        let pending = *refresh == RosterRefresh::Pending;
        if pending {
            self.started |= self.idle;
            return None;
        }
        self.idle = true;
        match roster {
            RosterStatus::Error(error) if self.started => Some(error.clone()),
            _ => None,
        }
    }
}

impl Session {
    fn work_ops(&self) -> Ops {
        self.ops()
    }

    async fn writer(&self, addition: bool) -> Result<Context, String> {
        let ops = self.work_ops();
        let ctx = ops.context().await;
        ctx.map_err(|_| {
            if addition {
                "This community cannot add agents to channels right now. Check the connection and retry.".to_owned()
            } else {
                "This community does not support channel creation yet. Your draft is kept here."
                    .to_owned()
            }
        })
    }

    /// Queues a new private work session and returns its create command
    /// (`workSessions.create`).
    pub async fn work_session_create(
        &self,
        id: &str,
        title: &str,
        parent: Option<&str>,
    ) -> Result<EventId, String> {
        let ctx = self.writer(false).await?;
        let draft = create_draft(id, title, parent)?;
        // The outbox persists on the I/O runtime, whichever executor runs this.
        let _runtime = self.runtime.enter();
        ctx.outbox
            .send(draft, SendOptions::default())
            .map_err(|error| error.to_string())
    }

    /// Invites the agent to a top-level session (`workSessions.invite`).
    pub async fn work_session_invite(
        &self,
        id: &str,
        pubkey: &str,
        rules: &AgentRules,
    ) -> Result<EventId, String> {
        if !uuid(id) {
            return Err("Invalid session identifier".into());
        }
        let target = listed(&self.snapshot.borrow(), id);
        if target.as_ref().is_none_or(|channel| {
            channel.channel_type != Some(ChannelType::Session)
                || channel.parent_channel_id.is_some()
        }) {
            return Err("Manage this session’s participants in its parent channel.".into());
        }
        if !is_key(pubkey) {
            return Err("Choose a valid participant.".into());
        }
        if !rules.selectable.iter().any(|key| key == pubkey) {
            return Err("Choose an agent from your agent library.".into());
        }
        let ctx = self.writer(false).await?;
        let _runtime = self.runtime.enter();
        ctx.outbox
            .send(add_draft(id, pubkey), SendOptions::default())
            .map_err(|error| error.to_string())
    }

    /// Waits until a work-session write is delivered, retrying a failed
    /// one (`workSessions.delivered`).
    pub async fn work_session_delivered(&self, id: EventId) -> Result<(), String> {
        let ops = self.work_ops();
        self.runtime
            .spawn(async move {
                let ctx = ops.context().await.map_err(|_| {
                    "The community connection cannot confirm this operation.".to_owned()
                })?;
                delivered(&ctx, id, false, true).await
            })
            .await
            .unwrap_or_else(|_| Err("The session has closed".into()))
    }

    /// Whether the operation failed for good (`workSessions.failed`).
    pub async fn work_session_failed(&self, id: EventId) -> bool {
        match self.work_ops().context().await {
            Ok(ctx) => ctx
                .outbox
                .get(&id)
                .is_some_and(|op| op.delivery == Delivery::Failed),
            Err(_) => false,
        }
    }

    /// Drops a failed operation so the draft can be edited and sent again.
    pub async fn work_session_discard(&self, id: EventId) -> Result<(), String> {
        let ctx = self.writer(false).await?;
        if !ctx
            .outbox
            .get(&id)
            .is_some_and(|op| op.delivery == Delivery::Failed)
        {
            return Err("This operation is still unconfirmed. Retry to confirm it first.".into());
        }
        let dismissed = {
            let _runtime = self.runtime.enter();
            ctx.outbox.dismiss(&id)
        };
        self.runtime
            .spawn(dismissed)
            .await
            .map_err(|_| "The session has closed".to_owned())?
            .map_err(|error| error.to_string())
    }

    /// Resolves once the channel list shows `id` with what `expected`
    /// asks for (`workSessions.refresh`). `session_only` also requires a
    /// session.
    pub async fn work_session_refresh(
        &self,
        id: &str,
        expected: Expected,
        session_only: bool,
    ) -> Result<(), String> {
        self.writer(!session_only).await?;
        let id = id.to_owned();
        let ops = self.work_ops();
        let mut snapshot = self.snapshot.clone();
        self.runtime
            .spawn(async move {
                let satisfied = |snapshot: &SessionSnapshot| {
                    snapshot.roster == RosterStatus::Ready
                        && listed(snapshot, &id)
                            .is_some_and(|channel| expected.satisfied(&channel, session_only))
                };
                let gate = async {
                    let mut scans = FreshScan::default();
                    loop {
                        {
                            let current = snapshot.borrow_and_update();
                            if let Some(error) =
                                scans.error(&current.live_status.roster, &current.roster)
                            {
                                return Err(error);
                            }
                            if satisfied(&current) {
                                return Ok(());
                            }
                        }
                        if snapshot.changed().await.is_err() {
                            return Err("The community connection changed.".to_owned());
                        }
                    }
                };
                let exact = async {
                    let ready = ops
                        .with(|actor| actor.roster == RosterStatus::Ready)
                        .await
                        .unwrap_or(false);
                    if ready {
                        // A failed or stale exact read leaves it to discovery.
                        let _ = exact_read(&ops, &id).await;
                    }
                    let _ = ops
                        .with(|actor| {
                            actor.start_scan(ScanOptions {
                                strong: true,
                                force_metadata: false,
                            })
                        })
                        .await;
                    futures::future::pending::<()>().await;
                };
                let result = tokio::time::timeout(CONFIRM_TIMEOUT, async {
                    tokio::select! {
                        result = gate => result,
                        () = exact => unreachable!("the exact read never finishes the gate"),
                    }
                })
                .await;
                result.unwrap_or_else(|_| {
                    Err(if session_only {
                        "Session saved, but its membership is still loading. Retry to continue."
                            .into()
                    } else {
                        "Agent addition is unconfirmed. Check channel membership before retrying; the request may already have reached the relay.".into()
                    })
                })
            })
            .await
            .unwrap_or_else(|_| Err("The session has closed".into()))
    }

    /// Channel ids whose relay-signed roster lists `pubkey`
    /// (`memberChannels`). Refuses a possibly truncated answer.
    pub async fn member_channels(&self, pubkey: &str) -> Result<Vec<String>, String> {
        let ops = self.work_ops();
        let pubkey = pubkey.to_owned();
        self.run(async move {
            let ctx = ops.context().await.map_err(|_| MEMBERSHIP_UNAVAILABLE)?;
            let events = foreground(
                &ctx,
                vec![Filter::from_iter([
                    ("kinds".to_owned(), json!([kinds::CHANNEL_MEMBERS])),
                    ("authors".to_owned(), json!([ctx.relay])),
                    ("#p".to_owned(), json!([pubkey])),
                    ("limit".to_owned(), json!(MEMBER_CHANNEL_LIMIT)),
                    ("consistency".to_owned(), json!("strong")),
                ])],
            )
            .await?;
            if events.len() >= MEMBER_CHANNEL_LIMIT {
                return Err("Too many channels to confirm. Retry later.".into());
            }
            let mut ids = Vec::new();
            for event in &events {
                if event.kind_u16() == kinds::CHANNEL_MEMBERS
                    && event.pubkey_hex() == ctx.relay
                    && event.has_tag("p", &pubkey)
                    && let Some(id) = event.tag("d")
                    && !ids.iter().any(|known| known == id)
                {
                    ids.push(id.to_owned());
                }
            }
            Ok(ids)
        })
        .await
    }

    /// Whether the channel's current relay-signed roster lists `pubkey`
    /// (`listsMember`). Missing evidence is an error, never absence.
    pub async fn lists_member(&self, id: &str, pubkey: &str) -> Result<bool, String> {
        let ops = self.work_ops();
        let (id, pubkey) = (id.to_owned(), pubkey.to_owned());
        self.run(async move {
            let ctx = ops.context().await.map_err(|_| MEMBERSHIP_UNAVAILABLE)?;
            let events = foreground(&ctx, vec![roster_filter(&ctx.relay, &id)]).await?;
            let roster = events
                .iter()
                .find(|event| {
                    event.kind_u16() == kinds::CHANNEL_MEMBERS
                        && event.pubkey_hex() == ctx.relay
                        && event.tag("d") == Some(id.as_str())
                })
                .ok_or(REFRESH_MEMBERSHIP)?;
            Ok(roster.has_tag("p", &pubkey))
        })
        .await
    }

    /// Re-reads one channel's roster and returns it as listed
    /// (`refreshMembership`).
    pub async fn refresh_membership(&self, id: &str) -> Result<ChannelSummary, String> {
        let ops = self.work_ops();
        let ctx = ops.context().await.map_err(|_| MEMBERSHIP_UNAVAILABLE)?;
        let events = foreground(&ctx, vec![roster_filter(&ctx.relay, id)]).await?;
        let found = events.iter().any(|event| {
            event.kind_u16() == kinds::CHANNEL_MEMBERS
                && event.pubkey_hex() == ctx.relay
                && event.tag("d") == Some(id)
        });
        ops.adopt(ctx.epoch, events).await?;
        let channel = {
            let id = id.to_owned();
            ops.with(move |actor| actor.discovery.as_ref().and_then(|d| d.get(&id)))
                .await?
        };
        match channel {
            Some(channel) if found && !channel.members.is_empty() && !channel.archived => {
                Ok(channel)
            }
            _ => Err(REFRESH_MEMBERSHIP.into()),
        }
    }

    /// Adds agents to a channel or session, and to a session's parent
    /// first (`workSessions.addAgents`).
    pub async fn add_agents(
        &self,
        id: &str,
        keys: &[String],
        rules: &AgentRules,
    ) -> Result<(), String> {
        let original = self.refresh_membership(id).await?;
        let mut seen = HashSet::new();
        let unique: Vec<String> = keys
            .iter()
            .filter(|key| seen.insert(key.as_str()))
            .filter(|key| {
                original.channel_type != Some(ChannelType::Session)
                    || !original.members.contains(key)
            })
            .cloned()
            .collect();
        if unique.is_empty() {
            return Ok(());
        }
        let parent_id = original
            .parent_channel_id
            .clone()
            .unwrap_or_else(|| id.to_owned());
        let parent = if parent_id == id {
            original.clone()
        } else {
            self.refresh_membership(&parent_id).await?
        };
        if parent.members.is_empty()
            || parent.archived
            || !matches!(
                parent.channel_type,
                Some(ChannelType::Stream | ChannelType::Forum | ChannelType::Session)
            )
        {
            return Err("Refresh the parent channel before adding agents.".into());
        }
        if unique.iter().any(|key| rules.archived.contains(key)) {
            return Err("This agent is archived. Choose another agent.".into());
        }
        if unique
            .iter()
            .any(|key| !parent.members.contains(key) && !rules.selectable.contains(key))
        {
            return Err("Choose an agent from your agent library.".into());
        }
        let targets: Vec<String> = if parent_id != id {
            vec![parent_id.clone(), id.to_owned()]
        } else {
            vec![parent_id.clone()]
        };
        let mut confirmations = Vec::new();
        for target in &targets {
            let mut added = Vec::new();
            for key in &unique {
                let ctx = self.writer(true).await?;
                let snapshot = self.snapshot.borrow().clone();
                if listed(&snapshot, id).and_then(|c| c.parent_channel_id)
                    != original.parent_channel_id
                {
                    return Err("This session moved. Refresh and retry.".into());
                }
                let current = listed(&snapshot, target)
                    .filter(|channel| !channel.members.is_empty() && !channel.archived)
                    .ok_or(MEMBERSHIP_UNAVAILABLE)?;
                if current.members.contains(key) {
                    continue;
                }
                let previous = ctx
                    .outbox
                    .visible()
                    .iter()
                    .find(|op| {
                        op.kind() == kinds::PUT_USER
                            && !op.acknowledged
                            && !matches!(op.delivery, Delivery::Accepted | Delivery::Seen)
                            && op.has_tag("h", target)
                            && op.has_tag("p", key)
                            && op.tag("role").is_none()
                    })
                    .cloned();
                if let Some(previous) = &previous
                    && now().saturating_sub(previous.created_at()) >= ADD_EXPIRY_SECS
                {
                    return Err(format!(
                        "This agent-add request expired. Open Outbox, remove the “Add agent {}” item, then add the agent again. Check channel membership first if the request was unconfirmed.",
                        &key[..12.min(key.len())]
                    ));
                }
                let operation = match previous {
                    Some(previous) => previous.id(),
                    None => {
                        let _runtime = self.runtime.enter();
                        ctx.outbox
                            .send(add_draft(target, key), SendOptions::default())
                            .map_err(|error| error.to_string())?
                    }
                };
                let ops = self.work_ops();
                self.runtime
                    .spawn(async move {
                        let ctx = ops.context().await?;
                        delivered(&ctx, operation, true, true).await
                    })
                    .await
                    .unwrap_or_else(|_| Err("The session has closed".into()))?;
                added.push(key.clone());
            }
            if !added.is_empty() {
                confirmations.push((target.clone(), added));
            }
        }
        let checks = confirmations
            .into_iter()
            .map(|(target, members)| async move {
                self.work_session_refresh(&target, Expected::members(members), false)
                    .await
            });
        for result in futures::future::join_all(checks).await {
            result?;
        }
        if original.channel_type == Some(ChannelType::Session) {
            let mut expected = Expected::members(unique);
            if original.parent_channel_id.is_some() {
                expected.parent = Some(parent_id);
            }
            self.work_session_refresh(id, expected, true).await?;
        }
        Ok(())
    }

    /// The id of the message just queued in `channel` with this content:
    /// the newest such send in the outbox. A session start saves it, so a
    /// retry confirms that message rather than sending another.
    pub async fn queued_message(&self, channel: &str, content: &str) -> Option<EventId> {
        let ctx = self.work_ops().context().await.ok()?;
        ctx.outbox
            .visible()
            .iter()
            .filter(|op| {
                op.kind() == kinds::CHAT
                    && op.has_tag("h", channel)
                    && op.event.content == content
                    && op.event.pubkey.to_hex() == ctx.viewer
            })
            .max_by_key(|op| op.created_at())
            .map(|op| op.id())
    }

    /// The ids of the viewer's sends the outbox holds now, so
    /// [`Self::queued_after`] can tell which one a send added.
    pub async fn queued_ids(&self) -> std::collections::HashSet<EventId> {
        let Ok(ctx) = self.work_ops().context().await else {
            return Default::default();
        };
        ctx.outbox.visible().iter().map(|op| op.id()).collect()
    }

    /// The message a send added to `channel`: the newest of the viewer's
    /// chat sends there that `known` did not hold.
    pub async fn queued_after(
        &self,
        channel: &str,
        known: &std::collections::HashSet<EventId>,
    ) -> Option<EventId> {
        let ctx = self.work_ops().context().await.ok()?;
        ctx.outbox
            .visible()
            .iter()
            .filter(|op| {
                op.kind() == kinds::CHAT
                    && op.has_tag("h", channel)
                    && op.event.pubkey.to_hex() == ctx.viewer
                    && !known.contains(&op.id())
            })
            .max_by_key(|op| op.created_at())
            .map(|op| op.id())
    }

    /// Reads the viewer's channel list again (`channels.refreshList`).
    pub fn refresh_channels(&self) {
        let _ = self.commands.send(super::Message::With(Box::new(|actor| {
            actor.start_scan(ScanOptions {
                strong: false,
                force_metadata: false,
            })
        })));
    }

    /// Reads the roster again from the relay's primary
    /// (`channels.refreshRoster`).
    pub fn refresh_roster(&self) {
        let _ = self.commands.send(super::Message::With(Box::new(|actor| {
            actor.start_scan(ScanOptions {
                strong: true,
                force_metadata: true,
            })
        })));
    }

    /// The channel list as last published.
    pub fn channels_now(&self) -> Arc<[ChannelSummary]> {
        self.snapshot.borrow().channels.clone()
    }

    /// Watches the session for a gate of its own.
    pub fn watch(&self) -> watch::Receiver<Arc<SessionSnapshot>> {
        self.snapshot.clone()
    }
}

fn roster_filter(relay: &str, id: &str) -> Filter {
    Filter::from_iter([
        ("kinds".to_owned(), json!([kinds::CHANNEL_MEMBERS])),
        ("authors".to_owned(), json!([relay])),
        ("#d".to_owned(), json!([id])),
        ("limit".to_owned(), json!(1)),
        ("consistency".to_owned(), json!("strong")),
    ])
}

async fn foreground(ctx: &Context, filters: Vec<Filter>) -> Result<Vec<Verified>, String> {
    ctx.reader
        .read(
            filters,
            ReadOptions {
                priority: Priority::Foreground,
                fresh: true,
            },
        )
        .await
        .map(|events| events.to_vec())
        .map_err(|error| error.message)
}

/// The exact read for one channel: its roster when listed, else its
/// metadata and roster through resolution.
async fn exact_read(ops: &Ops, id: &str) -> Result<(), String> {
    let ctx = ops.context().await?;
    let wanted = id.to_owned();
    let listed = ops
        .with(move |actor| {
            actor
                .discovery
                .as_ref()
                .is_some_and(|discovery| discovery.get(&wanted).is_some())
        })
        .await?;
    if listed {
        let events = read(&ctx, vec![roster_filter(&ctx.relay, id)]).await?;
        ops.adopt(ctx.epoch, events).await
    } else {
        ops.resolve(&ctx, vec![id.to_owned()]).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    const ID: &str = "11111111-1111-4111-8111-111111111111";
    const PARENT: &str = "22222222-2222-4222-8222-222222222222";

    #[test]
    fn ignores_a_roster_error_from_an_earlier_scan() {
        let stale = RosterStatus::Error("old".into());
        let mut scans = FreshScan::default();
        // The old error stays while idle, and while the new scan runs.
        assert_eq!(
            scans.error(&RosterRefresh::Error("old".into()), &stale),
            None
        );
        assert_eq!(scans.error(&RosterRefresh::Pending, &stale), None);
        let fresh = RosterStatus::Error("new".into());
        assert_eq!(
            scans.error(&RosterRefresh::Error("new".into()), &fresh),
            Some("new".into())
        );
        // A scan already running when the refresh began is not fresh.
        let mut scans = FreshScan::default();
        assert_eq!(scans.error(&RosterRefresh::Pending, &stale), None);
        assert_eq!(
            scans.error(&RosterRefresh::Error("old".into()), &stale),
            None
        );
    }

    #[test]
    fn create_draft_carries_session_metadata() {
        let draft = create_draft(ID, "Fix the build", Some(PARENT)).unwrap();
        assert_eq!(draft.kind, kinds::CREATE_CHANNEL);
        assert!(
            draft
                .tags
                .contains(&vec!["visibility".into(), "private".into()])
        );
        assert!(draft.tags.contains(&vec![
            "about".into(),
            format!("{SESSION_CHANNEL_DESCRIPTION}\nparent:{PARENT}")
        ]));
        assert_eq!(
            crate::discovery::session_metadata(Some(&session_description(Some(PARENT)))),
            Some(Some(PARENT.to_owned()))
        );
    }

    #[test]
    fn create_draft_refuses_bad_input() {
        assert_eq!(
            create_draft("nope", "x", None).unwrap_err(),
            "Invalid session identifier"
        );
        assert_eq!(
            create_draft(ID, " ", None).unwrap_err(),
            "Use a session title between 1 and 120 characters."
        );
        assert_eq!(
            create_draft(ID, &"x".repeat(121), None).unwrap_err(),
            "Use a session title between 1 and 120 characters."
        );
        assert_eq!(
            create_draft(ID, "x", Some(ID)).unwrap_err(),
            "A session cannot be its own parent."
        );
    }

    #[test]
    fn titles_fold_whitespace_and_cap_at_80() {
        assert_eq!(session_title("  hello\n\n  world "), "hello world");
        assert_eq!(session_title(&"é".repeat(100)).chars().count(), 80);
    }

    #[test]
    fn expectations_check_type_members_and_parent() {
        let channel = ChannelSummary {
            id: ID.into(),
            channel_type: Some(ChannelType::Session),
            members: vec!["a".into(), "b".into()],
            parent_channel_id: Some(PARENT.into()),
            ..Default::default()
        };
        assert!(Expected::member("a").satisfied(&channel, true));
        assert!(!Expected::member("c").satisfied(&channel, true));
        assert!(Expected::members(vec!["a".into(), "b".into()]).satisfied(&channel, true));
        assert!(Expected::parent(PARENT).satisfied(&channel, true));
        assert!(!Expected::parent(ID).satisfied(&channel, true));
        let stream = ChannelSummary {
            channel_type: Some(ChannelType::Stream),
            ..channel
        };
        assert!(!Expected::default().satisfied(&stream, true));
        assert!(Expected::default().satisfied(&stream, false));
    }
}
