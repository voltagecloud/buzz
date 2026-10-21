//! Adding people to channels and administering their roles
//! (`channel-members/members.ts`, `operations.ts`, `administration.ts`).
//!
//! Both are session-owned: closing the Members dialog never cancels an
//! addition, its state survives reopening, and an administration command
//! that may have reached the relay is never sent again until a readback
//! settles it.

use super::actions::Ops;
use super::delivery::Wait;
use super::{Actor, Session};
use crate::lifecycle::{self, CONFIRM_DELAYS, ChannelState, RUN_TIMEOUT};
use crate::members::{self, MemberChange};
use crate::outbox::{Delivery, Operation, Outbox, PublishError, Recovery, SendOptions};
use nostr::prelude::EventId;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const CLOSED: &str = "The community connection closed.";
const ACCESS: &str = "Channel access unavailable; refresh membership.";
const UNCERTAIN: &str = "This request may have taken effect. Refresh members and roles to check its result; it will not be sent again.";
/// A failed addition older than this is dismissed before a new one.
const ADDITION_EXPIRY_SECS: u64 = 15 * 60;
/// An addition waits this long for delivery and the roster.
const ADDITION_TIMEOUT: Duration = Duration::from_secs(60);

/// One person being added, as the Members dialog shows it after reopening.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberAddition {
    pub channel: String,
    pub pubkey: String,
    pub pending: bool,
    pub confirmed: bool,
    pub error: Option<String>,
}

/// `MemberAdditionIntent`: the outbox receipt an addition continues.
#[derive(Clone, Debug, Default)]
struct Intent {
    id: Option<EventId>,
    confirmed: bool,
    /// The failed receipt was explicitly dismissed: a fresh send is allowed.
    dismissed: bool,
    /// The receipt's last observed delivery.
    seen: Option<Delivery>,
}

pub(super) struct Addition {
    shown: MemberAddition,
    intent: Intent,
    /// Callers awaiting the addition in flight.
    done: Option<watch::Sender<Option<Result<(), String>>>>,
}

#[derive(Default)]
pub(super) struct Members {
    additions: HashMap<(String, String), Addition>,
    /// Channels with an administration command in flight.
    active: std::collections::HashSet<String>,
    operations: HashMap<String, MemberOperation>,
}

/// Where the latest role change or removal in a channel stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberOperation {
    pub change: MemberChange,
    pub status: MemberOperationStatus,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberOperationStatus {
    Pending,
    /// It may have taken effect; it is never sent again. Refresh to check.
    Uncertain,
    Confirmed,
    Failed,
}

/// Whether an addition carries the bot role tag.
#[derive(Clone, Copy)]
enum Role {
    /// None at all, as a template's setup adds.
    Plain,
    /// When their profile says they are an agent.
    Profile,
    /// They are one of the viewer's agents.
    Agent,
}

impl Session {
    /// Adds someone to a channel through the outbox and waits for the
    /// roster to list them. A second call for the same person joins the
    /// first; a failed or uncertain earlier addition is continued, never
    /// duplicated.
    pub async fn add_member(&self, channel: &str, pubkey: &str) -> Result<(), String> {
        self.addition(channel, pubkey, false, false, Role::Profile)
            .await
    }

    /// [`Self::add_member`] for someone known to be one of your agents,
    /// whose profile may not be loaded yet: added with the bot role
    /// (`addChannelMember`'s agent choices).
    pub async fn add_agent_member(&self, channel: &str, pubkey: &str) -> Result<(), String> {
        self.addition(channel, pubkey, false, false, Role::Agent)
            .await
    }

    /// [`Self::add_member`] with no role tag, as a channel template's
    /// setup adds its agents (`["h"], ["p"]` only).
    pub async fn add_member_plain(&self, channel: &str, pubkey: &str) -> Result<(), String> {
        self.addition(channel, pubkey, false, false, Role::Plain)
            .await
    }

    /// Adds someone to a private work session the viewer belongs to
    /// (`canShareSession`). Agents join through the session agent picker,
    /// which passes `allow_agents`.
    pub async fn add_session_participant(
        &self,
        channel: &str,
        pubkey: &str,
        allow_agents: bool,
    ) -> Result<(), String> {
        self.addition(channel, pubkey, true, allow_agents, Role::Profile)
            .await
    }

    /// Additions in `channel` the session still tracks: in flight, or
    /// failed with their error.
    pub async fn member_additions(&self, channel: &str) -> Vec<MemberAddition> {
        let channel = channel.to_owned();
        self.ops()
            .with(move |actor| {
                actor
                    .members
                    .additions
                    .values()
                    .filter(|addition| addition.shown.channel == channel)
                    .map(|addition| addition.shown.clone())
                    .collect()
            })
            .await
            .unwrap_or_default()
    }

    async fn addition(
        &self,
        channel: &str,
        pubkey: &str,
        participant: bool,
        allow_agents: bool,
        role: Role,
    ) -> Result<(), String> {
        let key = (channel.to_owned(), pubkey.to_owned());
        let ops = self.ops();
        // Begin to finish runs in its own task: a dropped caller never
        // leaves the addition marked in flight.
        self.runtime
            .spawn(async move {
                let started = {
                    let key = key.clone();
                    ops.with(move |actor| actor.begin_addition(key)).await?
                };
                let mut intent = match started {
                    Err(mut waiting) => {
                        let result = waiting
                            .wait_for(Option::is_some)
                            .await
                            .map_err(|_| CLOSED.to_owned())?
                            .clone();
                        return result.unwrap_or_else(|| Err(CLOSED.into()));
                    }
                    Ok(intent) => intent,
                };
                let (channel, pubkey) = &key;
                let work = ops.add_member(
                    channel,
                    pubkey,
                    participant,
                    allow_agents,
                    role,
                    &mut intent,
                );
                let result = tokio::time::timeout(ADDITION_TIMEOUT, work)
                    .await
                    .unwrap_or_else(|_| {
                        Err(
                            "Member addition interrupted. Reopen Members to check its status."
                                .into(),
                        )
                    });
                let reported = result.clone();
                let _ = ops
                    .with(move |actor| actor.finish_addition(key, intent, reported))
                    .await;
                result
            })
            .await
            .unwrap_or_else(|_| Err(CLOSED.to_owned()))
    }

    /// Changes a member's role or removes them, published directly. One
    /// operation per channel at a time; an uncertain one blocks the next
    /// until [`Session::refresh_members`] shows its result.
    pub async fn change_member(&self, channel: &str, change: MemberChange) -> Result<(), String> {
        let (ops, id) = (self.ops(), channel.to_owned());
        // Begin to finish runs in its own task: a dropped caller never
        // leaves the channel marked busy.
        self.runtime
            .spawn(async move {
                let begin = (id.clone(), change.clone());
                ops.with(move |actor| actor.begin_member_operation(&begin.0, begin.1))
                    .await??;
                let published = std::sync::atomic::AtomicBool::new(false);
                let work = ops.administer(&id, &change, &published);
                let result = tokio::time::timeout(RUN_TIMEOUT, work)
                    .await
                    .unwrap_or_else(|_| Err("The request timed out. Try again.".into()));
                let uncertain = result.is_err()
                    && published.load(std::sync::atomic::Ordering::SeqCst)
                    && !matches!(&result, Err(error) if error.starts_with(REFUSED));
                let result = match result {
                    Err(_) if uncertain => Err(UNCERTAIN.to_owned()),
                    Err(error) => Err(error.strip_prefix(REFUSED).unwrap_or(&error).to_owned()),
                    Ok(()) => Ok(()),
                };
                let reported = result.clone();
                let _ = ops
                    .with(move |actor| {
                        actor.finish_member_operation(&id, change, reported, uncertain)
                    })
                    .await;
                result
            })
            .await
            .unwrap_or_else(|_| Err(CLOSED.to_owned()))
    }

    /// The latest role change or removal in `channel`, if any.
    pub async fn member_operation(&self, channel: &str) -> Option<MemberOperation> {
        let channel = channel.to_owned();
        self.ops()
            .with(move |actor| actor.members.operations.get(&channel).cloned())
            .await
            .ok()
            .flatten()
    }

    /// Re-reads roles (`administration.refresh`): an uncertain change the
    /// roster now shows is confirmed and stops blocking.
    pub async fn refresh_members(&self, channel: &str) -> Result<(), String> {
        let (commands, id) = (self.commands.clone(), channel.to_owned());
        let busy = {
            let id = id.clone();
            self.ops()
                .with(move |actor| actor.members.active.contains(&id))
                .await?
        };
        if busy {
            return Ok(());
        }
        self.runtime
            .spawn(async move {
                let ops = Ops { commands };
                ops.assert_access(&id, ACCESS).await?;
                let (ctx, state) = ops.membership(&id).await?;
                ops.assert_access(&id, ACCESS).await?;
                let resolved = {
                    let state = state.clone();
                    let id = id.clone();
                    ops.with(move |actor| {
                        if let Some(operation) = actor.members.operations.get_mut(&id)
                            && operation.status == MemberOperationStatus::Uncertain
                        {
                            if members::change_confirmed(&state, &operation.change) {
                                operation.status = MemberOperationStatus::Confirmed;
                                operation.error = None;
                            } else {
                                operation.error = Some(UNCERTAIN.into());
                            }
                        }
                    })
                    .await
                };
                resolved?;
                ops.adopt(ctx, vec![state.metadata, state.roster]).await
            })
            .await
            .unwrap_or_else(|_| Err(CLOSED.to_owned()))
    }
}

/// Marks a relay refusal inside the administration task: final, never
/// uncertain.
const REFUSED: &str = "\u{1}";

/// The epoch reads started in, to fence adoption.
type Epoch = u64;

impl Ops {
    /// A fresh strong read of the channel's signed state.
    async fn membership(&self, channel: &str) -> Result<(Epoch, ChannelState), String> {
        let (reader, relay, epoch) = self
            .with(|actor| {
                actor
                    .relay
                    .as_ref()
                    .map(|relay| (actor.reader.clone(), relay.authority.clone(), actor.epoch))
                    .ok_or("Not connected to the relay yet")
            })
            .await??;
        let events = reader
            .read(
                lifecycle::state_filters(&relay, channel),
                crate::reader::ReadOptions {
                    fresh: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.message)?;
        let state =
            ChannelState::parse(&events, channel, &relay).map_err(|error| error.to_string())?;
        Ok((epoch, state))
    }

    /// The addition preconditions (`check` in `addChannelMember`).
    async fn check_addition(
        &self,
        channel: &str,
        pubkey: &str,
        participant: bool,
        allow_agents: bool,
    ) -> Result<(), String> {
        let (id, key) = (channel.to_owned(), pubkey.to_owned());
        self.with(move |actor| {
            let viewer = actor.identity.pubkey_hex();
            let summary = actor.discovery.as_ref().and_then(|d| d.get(&id));
            let allowed = summary.as_ref().is_some_and(|summary| {
                if participant {
                    members::can_share_session(summary, &viewer)
                } else {
                    let member = summary.members.contains(&viewer);
                    members::can_add_members(summary, member)
                }
            });
            if !allowed {
                return Err(
                    "This channel cannot add members right now. Refresh and try again.".into(),
                );
            }
            let agent = actor
                .profiles
                .snapshot()
                .get(&key)
                .is_some_and(|profile| profile.is_agent);
            if participant && !allow_agents && agent {
                return Err("Add agents with the session agent picker instead.".into());
            }
            Ok(())
        })
        .await?
    }

    /// `addChannelMember`.
    async fn add_member(
        &self,
        channel: &str,
        pubkey: &str,
        participant: bool,
        allow_agents: bool,
        role: Role,
        intent: &mut Intent,
    ) -> Result<(), String> {
        if !members::is_key(pubkey) {
            return Err("Choose a valid person or agent.".into());
        }
        self.check_addition(channel, pubkey, participant, allow_agents)
            .await?;
        let outbox: Outbox = self
            .with(|actor| actor.outbox.clone())
            .await?
            .ok_or("Member additions are unavailable.")?;
        outbox.ready().await.map_err(|error| error.to_string())?;
        self.check_addition(channel, pubkey, participant, allow_agents)
            .await?;
        let (epoch, state) = self.membership(channel).await?;
        self.adopt(epoch, vec![state.metadata.clone(), state.roster.clone()])
            .await?;
        self.check_addition(channel, pubkey, participant, allow_agents)
            .await?;
        let member = state.role(pubkey).is_some();
        // Only a roster-confirmed earlier addition may be superseded.
        let fresh = intent.dismissed || (intent.confirmed && !member);
        let pending: Option<Arc<Operation>> = if fresh {
            None
        } else {
            outbox
                .visible()
                .iter()
                .find(|op| match intent.id {
                    Some(id) => op.id() == id,
                    None => !op.acknowledged && members::is_addition(op, channel, pubkey),
                })
                .cloned()
        };
        if pending.as_ref().is_some_and(|op| op.guarded) && !member {
            return Err(
                "Retry this invitation from the agent’s profile so its eligibility can be checked."
                    .into(),
            );
        }
        let recovery = Recovery {
            key: members::addition_key(channel, pubkey),
            value: pubkey.to_owned(),
        };
        if let Some(op) = pending.as_ref().filter(|op| !op.acknowledged) {
            outbox
                .recover(&op.id(), recovery.clone())
                .await
                .map_err(|error| error.to_string())?;
        }
        self.check_addition(channel, pubkey, participant, allow_agents)
            .await?;
        if member {
            if let Some(op) = &pending {
                intent.id.get_or_insert(op.id());
                outbox
                    .acknowledge(&op.id())
                    .await
                    .map_err(|error| error.to_string())?;
            }
            intent.confirmed = true;
            return Ok(());
        }
        if !fresh && intent.id.is_some() && pending.is_none() {
            return Err(
                "The original addition receipt is unavailable. Refresh membership before trying again."
                    .into(),
            );
        }
        if pending.as_ref().is_some_and(|op| {
            op.delivery == Delivery::Failed
                && now().saturating_sub(op.created_at()) >= ADDITION_EXPIRY_SECS
        }) {
            return Err(
                "This addition expired. Remove its Not sent item in Outbox, then try again.".into(),
            );
        }
        let id = match &pending {
            Some(op) => {
                if matches!(op.delivery, Delivery::Failed | Delivery::Unknown) {
                    outbox
                        .retry(&op.id(), None)
                        .map_err(|error| error.to_string())?;
                }
                op.id()
            }
            None => {
                let key = pubkey.to_owned();
                let agent = self
                    .with(move |actor| {
                        actor
                            .profiles
                            .snapshot()
                            .get(&key)
                            .is_some_and(|profile| profile.is_agent)
                    })
                    .await?;
                let bot = match role {
                    Role::Plain => false,
                    Role::Profile => agent,
                    Role::Agent => true,
                };
                let draft = members::add(channel, pubkey, bot).map_err(|e| e.to_string())?;
                outbox
                    .send(
                        draft,
                        SendOptions {
                            recovery: Some(recovery),
                            gate: None,
                        },
                    )
                    .map_err(|error| error.to_string())?
            }
        };
        intent.id = Some(id);
        intent.dismissed = false;
        intent.confirmed = false;
        delivered(&outbox, id).await?;
        self.check_addition(channel, pubkey, participant, allow_agents)
            .await?;
        // Delivery alone is not membership: the signed roster must list them.
        let mut confirmed = None;
        for delay in CONFIRM_DELAYS {
            tokio::time::sleep(delay).await;
            let (epoch, state) = self.membership(channel).await?;
            if state.role(pubkey).is_some() {
                confirmed = Some((epoch, state));
                break;
            }
        }
        let Some((epoch, state)) = confirmed else {
            return Err(
                "Addition is not confirmed in the member list yet. Retry to check again.".into(),
            );
        };
        self.adopt(epoch, vec![state.metadata, state.roster])
            .await?;
        self.check_addition(channel, pubkey, participant, allow_agents)
            .await?;
        outbox
            .acknowledge(&id)
            .await
            .map_err(|error| error.to_string())?;
        intent.confirmed = true;
        Ok(())
    }

    /// `administration.run`: authorized from fresh roles before and after
    /// signing, then confirmed by readback.
    async fn administer(
        &self,
        channel: &str,
        change: &MemberChange,
        published: &std::sync::atomic::AtomicBool,
    ) -> Result<(), String> {
        let (reader, http, identity) = self
            .with(|actor| {
                actor
                    .relay
                    .as_ref()
                    .map(|_| {
                        (
                            actor.reader.clone(),
                            actor.http.clone(),
                            actor.identity.clone(),
                        )
                    })
                    .ok_or("Member administration is unavailable on this connection.")
            })
            .await??;
        let viewer = identity.pubkey_hex();
        let mut owner: Option<Option<String>> = None;
        let authorize = async |owner: &mut Option<Option<String>>| -> Result<(), String> {
            self.assert_access(channel, ACCESS).await?;
            let (_, state) = self.membership(channel).await?;
            self.assert_access(channel, ACCESS).await?;
            // Removing someone's own agent needs its attestation, read
            // fresh once per attempt.
            if owner.is_none()
                && change.change == members::Change::Remove
                && !state.can_manage_member(&viewer, &change.pubkey)
                && state.can_remove_member(&viewer, &change.pubkey, Some(&viewer))
            {
                let events = reader
                    .read(
                        vec![members::owner_filter(&change.pubkey)],
                        crate::reader::ReadOptions {
                            fresh: true,
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|error| error.message)?;
                self.assert_access(channel, ACCESS).await?;
                *owner = Some(
                    crate::account::newest_profile(&events, &change.pubkey)
                        .and_then(crate::profiles::attested_owner),
                );
            }
            members::authorize(&state, &viewer, change, owner.clone().flatten().as_deref())
                .map_err(|e| e.to_string())
        };
        authorize(&mut owner).await?;
        let template = members::administration(channel, change, &viewer, now())
            .map_err(|error| error.to_string())?;
        let unsigned = identity
            .template(
                buzz_identity::Purpose::Content,
                template.kind,
                template.created_at,
                template.tags,
                template.content,
            )
            .map_err(|error| error.to_string())?;
        let event = identity
            .sign(buzz_identity::Purpose::Content, unsigned)
            .map_err(|error| error.to_string())?;
        authorize(&mut owner).await?;
        published.store(true, std::sync::atomic::Ordering::SeqCst);
        let body = serde_json::to_string(&event).expect("events serialize");
        match crate::outbox::accept_publish(http.post("/events", body).await, &event.id) {
            Ok(_) | Err(PublishError::Uncertain(_)) => {}
            Err(PublishError::Rejected(message)) => return Err(format!("{REFUSED}{message}")),
        }
        for delay in CONFIRM_DELAYS {
            tokio::time::sleep(delay).await;
            let (epoch, state) = self.membership(channel).await?;
            if members::change_confirmed(&state, change) {
                return self.adopt(epoch, vec![state.metadata, state.roster]).await;
            }
        }
        Err(UNCERTAIN.into())
    }
}

/// `waitForAddition`: delivered, or its failure. Delivery is not
/// membership; the roster is read next.
async fn delivered(outbox: &Outbox, id: EventId) -> Result<(), String> {
    let wait = Wait {
        missing: None,
        failed: "Addition is not confirmed. Retry to check again.",
        unknown_fails: true,
        closed: CLOSED,
    };
    super::delivery::delivered(outbox, id, &wait).await
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

type Waiter = watch::Receiver<Option<Result<(), String>>>;

impl Members {
    /// Starts an addition, or returns the one in flight to wait on.
    fn begin_addition(&mut self, key: (String, String)) -> Result<Intent, Waiter> {
        let entry = self
            .additions
            .entry(key.clone())
            .or_insert_with(|| Addition {
                shown: MemberAddition {
                    channel: key.0.clone(),
                    pubkey: key.1.clone(),
                    pending: false,
                    confirmed: false,
                    error: None,
                },
                intent: Intent::default(),
                done: None,
            });
        if let Some(done) = &entry.done {
            return Err(done.subscribe());
        }
        entry.done = Some(watch::channel(None).0);
        entry.shown.pending = true;
        entry.shown.error = None;
        Ok(entry.intent.clone())
    }

    fn finish_addition(
        &mut self,
        key: (String, String),
        intent: Intent,
        result: Result<(), String>,
    ) {
        let Some(entry) = self.additions.get_mut(&key) else {
            return;
        };
        if let Some(done) = entry.done.take() {
            done.send_replace(Some(result.clone()));
        }
        entry.intent = intent;
        entry.shown.pending = false;
        match result {
            // The intent is kept so a later Add can supersede it.
            Ok(()) => {
                entry.shown.confirmed = true;
                entry.shown.error = None;
            }
            Err(error) => {
                entry.shown.confirmed = entry.intent.confirmed;
                entry.shown.error = Some(error);
            }
        }
    }

    /// `trackMemberAdditionReceipts`: a failed receipt that disappears
    /// was dismissed in the Outbox, so a fresh send is allowed. Accepted
    /// or unknown receipts that disappear never are.
    fn track(&mut self, delivery: impl Fn(&EventId) -> Option<Delivery>) {
        for addition in self.additions.values_mut() {
            let intent = &mut addition.intent;
            let Some(id) = intent.id else {
                continue;
            };
            match delivery(&id) {
                Some(seen) => intent.seen = Some(seen),
                None if !intent.confirmed && intent.seen == Some(Delivery::Failed) => {
                    intent.seen = None;
                    intent.dismissed = true;
                }
                None => {}
            }
        }
    }

    fn begin_operation(
        &mut self,
        channel: &str,
        change: MemberChange,
        accessible: bool,
    ) -> Result<(), String> {
        if self
            .operations
            .get(channel)
            .is_some_and(|operation| operation.status == MemberOperationStatus::Uncertain)
        {
            return Err(UNCERTAIN.into());
        }
        if !accessible {
            return Err(ACCESS.into());
        }
        if !self.active.insert(channel.to_owned()) {
            return Err("A member operation is still in progress.".into());
        }
        self.operations.insert(
            channel.to_owned(),
            MemberOperation {
                change,
                status: MemberOperationStatus::Pending,
                error: None,
            },
        );
        Ok(())
    }

    fn finish_operation(
        &mut self,
        channel: &str,
        change: MemberChange,
        result: Result<(), String>,
        uncertain: bool,
    ) {
        self.active.remove(channel);
        let (status, error) = match result {
            Ok(()) => (MemberOperationStatus::Confirmed, None),
            Err(error) if uncertain => (MemberOperationStatus::Uncertain, Some(error)),
            Err(error) => (MemberOperationStatus::Failed, Some(error)),
        };
        self.operations.insert(
            channel.to_owned(),
            MemberOperation {
                change,
                status,
                error,
            },
        );
    }

    /// Role authority goes, but an uncertain command stays fenced
    /// (`administration.clear`).
    fn clear(&mut self) {
        self.operations
            .retain(|_, operation| operation.status == MemberOperationStatus::Uncertain);
    }
}

impl Actor {
    fn begin_addition(&mut self, key: (String, String)) -> Result<Intent, Waiter> {
        self.members.begin_addition(key)
    }

    fn finish_addition(
        &mut self,
        key: (String, String),
        intent: Intent,
        result: Result<(), String>,
    ) {
        self.members.finish_addition(key, intent, result);
    }

    pub(super) fn track_additions(&mut self) {
        if let Some(outbox) = &self.outbox {
            self.members
                .track(|id| outbox.get(id).map(|op| op.delivery));
        }
    }

    fn begin_member_operation(
        &mut self,
        channel: &str,
        change: MemberChange,
    ) -> Result<(), String> {
        if self.relay.is_none() {
            return Err("Member administration is unavailable on this connection.".into());
        }
        let accessible = self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.can_access(channel));
        self.members.begin_operation(channel, change, accessible)
    }

    fn finish_member_operation(
        &mut self,
        channel: &str,
        change: MemberChange,
        result: Result<(), String>,
        uncertain: bool,
    ) {
        self.members
            .finish_operation(channel, change, result, uncertain);
    }

    /// Access changed or the cache was cleared.
    pub(super) fn members_clear(&mut self) {
        self.members.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::MemberRole;

    fn key() -> (String, String) {
        ("channel".into(), "ab".repeat(32))
    }

    #[tokio::test]
    async fn a_second_add_joins_the_first_and_state_survives() {
        let mut members = Members::default();
        let intent = members.begin_addition(key()).unwrap();
        assert!(intent.id.is_none());
        let mut waiting = members.begin_addition(key()).unwrap_err();
        let shown = &members.additions[&key()].shown;
        assert!(shown.pending && !shown.confirmed);
        let failed = Intent {
            id: Some(EventId::from_byte_array([0; 32])),
            ..Intent::default()
        };
        members.finish_addition(key(), failed, Err("Not sent".into()));
        let result = waiting.wait_for(Option::is_some).await.unwrap().clone();
        assert_eq!(result, Some(Err("Not sent".into())));
        let shown = &members.additions[&key()].shown;
        assert_eq!(shown.error.as_deref(), Some("Not sent"));
        // Reopening continues the same receipt.
        let again = members.begin_addition(key()).unwrap();
        assert_eq!(again.id, Some(EventId::from_byte_array([0; 32])));
    }

    #[test]
    fn only_a_dismissed_failure_allows_a_fresh_send() {
        let mut members = Members::default();
        let intent = Intent {
            id: Some(EventId::from_byte_array([0; 32])),
            ..Intent::default()
        };
        members.begin_addition(key()).unwrap();
        members.finish_addition(key(), intent, Err("refused".into()));
        // An accepted receipt that disappears never authorizes a resend.
        members.track(|_| Some(Delivery::Accepted));
        members.track(|_| None);
        assert!(!members.additions[&key()].intent.dismissed);
        members.track(|_| Some(Delivery::Failed));
        members.track(|_| None);
        assert!(members.additions[&key()].intent.dismissed);
    }

    #[test]
    fn administration_is_single_flight_and_uncertain_blocks() {
        let mut members = Members::default();
        let change = MemberChange {
            pubkey: "cd".repeat(32),
            expected: MemberRole::Member,
            change: members::Change::Admin,
        };
        assert_eq!(
            members.begin_operation("c", change.clone(), false),
            Err(ACCESS.into())
        );
        members.begin_operation("c", change.clone(), true).unwrap();
        assert_eq!(
            members.begin_operation("c", change.clone(), true),
            Err("A member operation is still in progress.".into())
        );
        members.finish_operation("c", change.clone(), Err(UNCERTAIN.into()), true);
        assert_eq!(
            members.begin_operation("c", change.clone(), true),
            Err(UNCERTAIN.into())
        );
        // Clearing keeps the fence.
        members.clear();
        assert_eq!(
            members.operations["c"].status,
            MemberOperationStatus::Uncertain
        );
    }
}
