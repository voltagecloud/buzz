//! Sharing a work session (`sessions/share.ts`, `share-attempt.ts`):
//! moving a Me conversation to Messages in place, or posting a link to it
//! in another channel, after granting the chosen people access.
//!
//! A submitted attempt belongs to the session, not to the dialog that
//! started it, so another view (or the same one reopened) resumes it with
//! the exact people chosen. Channel creation, member additions, details
//! saves and the outbox stay the durable owners of each operation; a
//! retry continues them and never repeats one that may have taken effect.

use super::Session;
use super::delivery::{Wait, delivered};
use crate::discovery::{ChannelSummary, ChannelType};
use crate::event::kinds;
use crate::lifecycle::{AttemptStatus, ChannelDetails, DetailsDraft, NewChannel};
use crate::members;
use crate::messages::{self, Compose, Mentions};
use crate::outbox::{Delivery, Operation, Outbox, Recovery, SendOptions};
use crate::reader::ReadOptions;
use crate::socket::Filter;
use nostr::prelude::EventId;
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// People per group (`Choose at most 100 …`).
pub const MAX_PEOPLE: usize = 100;
/// How long a posted link may wait for its receipt.
const LINK_TIMEOUT: Duration = Duration::from_secs(15);

pub const IN_PROGRESS: &str = "This share is already in progress in another view.";
pub const CHANGED: &str = "This share finished or changed. Close and reopen to start a new share.";
const LINK_UNCONFIRMED: &str = "Link delivery is unconfirmed. Check Outbox before retrying.";
const ACCESS_CHANGED: &str = "Session or destination access changed. Refresh and retry.";

/// `buzz://channel/<id>`, the id percent-encoded as `encodeURIComponent`
/// does.
pub fn session_link(id: &str) -> String {
    let mut encoded = String::with_capacity(id.len());
    for byte in id.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    format!("buzz://channel/{encoded}")
}

/// `[Session · <first 8>](buzz://channel/<id>)`.
pub fn session_link_message(id: &str) -> String {
    let short: String = id.chars().take(8).collect();
    format!("[Session · {short}]({})", session_link(id))
}

/// The outbox recovery a link post carries, unique per source and
/// destination.
pub fn link_recovery(source: &str, destination: &str) -> Recovery {
    Recovery {
        key: format!("session-share-link:{source}:{destination}"),
        value: "1".into(),
    }
}

/// `isSessionLinkReceipt`: an outbox item is exactly the link a share
/// posted, so Outbox may finish it after its attempt was reset.
pub fn is_session_link_receipt(op: &Operation) -> bool {
    let Some(recovery) = &op.recovery else {
        return false;
    };
    let Some(rest) = recovery.key.strip_prefix("session-share-link:") else {
        return false;
    };
    let mut parts = rest.split(':');
    let (Some(source), Some(destination), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !source.is_empty()
        && !destination.is_empty()
        && recovery.value == "1"
        && op.kind() == kinds::CHAT
        && op.event.content == session_link_message(source)
        && op.has_tag("h", destination)
}

/// Who gets session access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// The destination's members, read fresh once and then frozen.
    Everyone,
    Selected,
}

/// Where the share goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// An existing channel, or the source itself for a direct share.
    Channel(String),
    /// "Create new channel…".
    New,
}

/// `ShareIntent`: what was chosen when the share was submitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareIntent {
    pub destination: Destination,
    pub audience: Audience,
    /// A new channel's canonical name and privacy.
    pub name: Option<String>,
    pub private: Option<bool>,
    /// A direct share's details and the details they were edited from.
    pub details: Option<DetailsDraft>,
    pub details_base: Option<ChannelDetails>,
    /// People added to a new channel.
    pub channel_people: Vec<String>,
    /// People granted session access.
    pub session_people: Vec<String>,
}

impl ShareIntent {
    /// A share that moves `source` to Messages in place.
    pub fn is_direct(&self, source: &str) -> bool {
        self.destination == Destination::Channel(source.to_owned())
    }
}

/// What an attempt has done so far.
#[derive(Clone, Debug, Default)]
struct Progress {
    /// A created channel stays the destination on every retry.
    created: Option<String>,
    /// The id chosen for the channel before its creation was sent, so a
    /// retry after an uncertain creation never makes a second channel.
    creating: Option<String>,
    details_applied: bool,
    message_id: Option<EventId>,
    /// People whose session access is confirmed.
    granted: HashSet<String>,
    /// The destination roster, frozen before any grant.
    audience_keys: Option<Vec<String>>,
    /// Two views never run competing link operations.
    running: bool,
}

/// `ShareAttempt`.
#[derive(Debug)]
pub struct ShareAttempt {
    pub intent: ShareIntent,
    progress: Mutex<Progress>,
}

impl ShareAttempt {
    fn progress(&self) -> std::sync::MutexGuard<'_, Progress> {
        self.progress.lock().expect("share attempt lock")
    }

    pub fn running(&self) -> bool {
        self.progress().running
    }

    /// The channel this attempt created, if any.
    pub fn created(&self) -> Option<String> {
        self.progress().created.clone()
    }

    /// The id this attempt creates its channel with, chosen once.
    fn creating(&self) -> String {
        self.progress()
            .creating
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone()
    }

    /// The channel a retry continues in.
    pub fn target(&self) -> Option<String> {
        self.created().or_else(|| match &self.intent.destination {
            Destination::Channel(id) => Some(id.clone()),
            Destination::New => None,
        })
    }
}

/// One step of a share, in the order [`steps`] plans them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    CreateChannel,
    /// Adds this person to the new channel.
    AddToNewChannel(String),
    CheckMembers,
    ConfirmAccess,
    SaveDetails,
    MoveToMessages,
    PostLink,
}

impl Step {
    /// The progress line, with `name` naming a person.
    pub fn label(&self, name: impl Fn(&str) -> String) -> String {
        match self {
            Self::CreateChannel => "Creating channel…".into(),
            Self::AddToNewChannel(key) => format!("Adding {} to the new channel…", name(key)),
            Self::CheckMembers => "Checking channel members…".into(),
            Self::ConfirmAccess => "Confirming session access…".into(),
            Self::SaveDetails => "Saving channel settings…".into(),
            Self::MoveToMessages => "Moving to Messages…".into(),
            Self::PostLink => "Posting session link…".into(),
        }
    }
}

/// The steps a run of `intent` takes from `source`: create and fill a new
/// channel, freeze the destination roster (once), grant access, then
/// either move the source to Messages or post the link.
pub fn steps(intent: &ShareIntent, source: &str, audience_known: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    if intent.destination == Destination::New {
        steps.push(Step::CreateChannel);
        steps.extend(
            intent
                .channel_people
                .iter()
                .cloned()
                .map(Step::AddToNewChannel),
        );
    }
    if intent.audience == Audience::Everyone && !audience_known {
        steps.push(Step::CheckMembers);
    }
    steps.push(Step::ConfirmAccess);
    if intent.is_direct(source) {
        steps.extend([Step::SaveDetails, Step::MoveToMessages]);
    } else {
        steps.push(Step::PostLink);
    }
    steps
}

/// The error a failed run shows: the step's message, then what already
/// took effect and stays.
pub fn failure_text(
    message: &str,
    details_applied: bool,
    in_messages: bool,
    confirmed: usize,
) -> String {
    let mut text = message.to_owned();
    if details_applied {
        text.push_str(" Channel settings were saved; retry to finish moving to Messages.");
    }
    if in_messages {
        text.push_str(" Conversation is in Messages; access already granted is not revoked.");
    }
    if confirmed > 0 {
        text.push_str(&format!(
            " {confirmed} session participant{} confirmed.",
            if confirmed == 1 { "" } else { "s" }
        ));
    }
    text
}

/// A destination roster as the "Everyone in this channel" audience: the
/// viewer's fellow members, at most 100, none archived.
pub fn audience_keys(
    destination: &ChannelSummary,
    viewer: &str,
    archived: &HashSet<String>,
) -> Result<Vec<String>, String> {
    if destination.cached
        || destination.archived
        || destination.read_only
        || !destination.members.iter().any(|key| key == viewer)
        || !matches!(
            destination.channel_type,
            Some(ChannelType::Stream | ChannelType::Forum)
        )
    {
        return Err("Destination membership could not be confirmed. Retry to share.".into());
    }
    let mut seen = HashSet::new();
    let keys: Vec<String> = destination
        .members
        .iter()
        .filter(|key| *key != viewer && seen.insert(key.as_str()))
        .cloned()
        .collect();
    if keys.len() > MAX_PEOPLE {
        return Err(
            "This channel has more than 100 recipients. Choose selected people instead.".into(),
        );
    }
    if keys.iter().any(|key| !members::is_key(key)) {
        return Err("Destination membership contains an invalid identity.".into());
    }
    if keys.iter().any(|key| archived.contains(key)) {
        return Err("A destination member is archived. Choose selected people instead.".into());
    }
    Ok(keys)
}

/// Whether the viewer may still share the source, or post in the
/// destination.
fn eligible(channel: &ChannelSummary, viewer: &str, session: bool) -> bool {
    !viewer.is_empty()
        && !channel.cached
        && !channel.read_only
        && !channel.archived
        && channel.members.iter().any(|key| key == viewer)
        && if session {
            members::can_share_session(channel, viewer)
        } else {
            matches!(
                channel.channel_type,
                Some(ChannelType::Stream | ChannelType::Forum)
            )
        }
}

/// Whether `op` is the viewer's link to `source` in `destination`.
fn is_link(op: &Operation, viewer: &str, source: &str, destination: &str) -> bool {
    op.kind() == kinds::CHAT
        && op.event.pubkey.to_hex() == viewer
        && op.event.content == session_link_message(source)
        && op.has_tag("h", destination)
}

/// What a run needs from the view that started it.
#[derive(Clone, Debug, Default)]
pub struct ShareOptions {
    /// The viewer's own agent identities; profiles flag the rest.
    pub known_agents: HashSet<String>,
    /// Identities the community archived.
    pub archived: HashSet<String>,
}

/// Clears `running` however the run ends.
struct Running(Arc<ShareAttempt>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.progress().running = false;
    }
}

impl Session {
    /// The share of `source` this session still owns, if any.
    pub fn share_attempt(&self, source: &str) -> Option<Arc<ShareAttempt>> {
        self.shares.lock().ok()?.get(source).cloned()
    }

    /// Starts a share of `source`, or returns the one already submitted:
    /// a saved attempt is never replaced by new choices.
    pub fn begin_share(&self, source: &str, intent: ShareIntent) -> Arc<ShareAttempt> {
        let mut shares = self.shares.lock().expect("shares lock");
        shares
            .entry(source.to_owned())
            .or_insert_with(|| {
                Arc::new(ShareAttempt {
                    intent,
                    progress: Mutex::default(),
                })
            })
            .clone()
    }

    /// Forgets the attempt; what it already did stays done.
    pub fn finish_share(&self, source: &str) {
        if let Ok(mut shares) = self.shares.lock() {
            shares.remove(source);
        }
    }

    /// Runs (or resumes) a submitted share step by step, reporting each
    /// step as it starts. On success the attempt is finished; on failure
    /// it stays for a retry, and the error says what already took effect.
    pub async fn run_share(
        &self,
        source: &str,
        attempt: &Arc<ShareAttempt>,
        options: &ShareOptions,
        on_step: impl Fn(&Step),
    ) -> Result<(), String> {
        {
            let mut progress = attempt.progress();
            if progress.running {
                return Err(IN_PROGRESS.into());
            }
            progress.running = true;
        }
        let _running = Running(attempt.clone());
        let direct = attempt.intent.is_direct(source);
        let mut confirmed = 0;
        let result = self
            .share_steps(source, attempt, options, &on_step, &mut confirmed)
            .await;
        match result {
            Ok(()) => {
                self.finish_share(source);
                Ok(())
            }
            Err(message) => {
                let kit = self.me_kit();
                let in_messages = direct && kit.snapshot().ready() && !kit.placed(source);
                let applied = attempt.progress().details_applied;
                Err(failure_text(&message, applied, in_messages, confirmed))
            }
        }
    }

    async fn share_steps(
        &self,
        source: &str,
        attempt: &Arc<ShareAttempt>,
        options: &ShareOptions,
        on_step: &impl Fn(&Step),
        confirmed: &mut usize,
    ) -> Result<(), String> {
        let intent = &attempt.intent;
        let direct = intent.is_direct(source);
        if intent.session_people.len() > MAX_PEOPLE || intent.channel_people.len() > MAX_PEOPLE {
            return Err("Choose at most 100 people in each group.".into());
        }
        if intent
            .session_people
            .iter()
            .chain(&intent.channel_people)
            .any(|key| self.known_agent(key, options))
        {
            return Err("Add agents from their own channel or session controls instead.".into());
        }
        let audience_known = attempt.progress().audience_keys.is_some();
        let mut target = attempt.target();
        for step in steps(intent, source, audience_known) {
            on_step(&step);
            match step {
                Step::CreateChannel => {
                    let (Some(name), Some(private)) = (&intent.name, intent.private) else {
                        return Err("Enter a channel name.".into());
                    };
                    let created = match attempt.created() {
                        Some(id) => id,
                        None => {
                            let id = attempt.creating();
                            self.create_channel_as(
                                NewChannel {
                                    name: name.clone(),
                                    description: String::new(),
                                    private,
                                    ttl: None,
                                    labels: Vec::new(),
                                },
                                id,
                            )
                            .await?
                        }
                    };
                    attempt.progress().created = Some(created.clone());
                    target = Some(created);
                }
                Step::AddToNewChannel(key) => {
                    if self.known_agent(&key, options) {
                        return Err("Add agents separately.".into());
                    }
                    let channel = target.clone().ok_or("Enter a channel name.")?;
                    self.add_member(&channel, &key).await?;
                }
                Step::CheckMembers => {
                    let channel = target.clone().ok_or("Choose a channel.")?;
                    let keys = self.share_audience(&channel, &options.archived).await?;
                    attempt.progress().audience_keys = Some(keys);
                }
                Step::ConfirmAccess => {
                    let people = match intent.audience {
                        Audience::Everyone => {
                            attempt.progress().audience_keys.clone().unwrap_or_default()
                        }
                        Audience::Selected if direct => {
                            let granted = attempt.progress().granted.clone();
                            intent
                                .session_people
                                .iter()
                                .filter(|key| !granted.contains(*key))
                                .cloned()
                                .collect()
                        }
                        Audience::Selected => intent.session_people.clone(),
                    };
                    let participant = !direct || self.is_session_channel(source);
                    self.grant_session_access(
                        source,
                        &people,
                        attempt,
                        intent.audience == Audience::Everyone,
                        participant,
                        confirmed,
                    )
                    .await?;
                }
                Step::SaveDetails => {
                    let (Some(details), Some(base)) = (&intent.details, &intent.details_base)
                    else {
                        return Err("Reload channel details before sharing.".into());
                    };
                    self.apply_shared_details(source, details, &base.draft)
                        .await?;
                    attempt.progress().details_applied = true;
                }
                Step::MoveToMessages => {
                    self.me_kit().set_placement(source, false, None).await?;
                }
                Step::PostLink => {
                    let channel = target.clone().ok_or("Choose a channel.")?;
                    let previous = attempt.progress().message_id;
                    self.publish_session_link(source, &channel, previous, |id| {
                        attempt.progress().message_id = Some(id);
                    })
                    .await?;
                }
            }
        }
        Ok(())
    }

    fn known_agent(&self, key: &str, options: &ShareOptions) -> bool {
        options.known_agents.contains(key)
            || self
                .snapshot
                .borrow()
                .profiles
                .get(key)
                .is_some_and(|profile| profile.is_agent)
    }

    fn is_session_channel(&self, id: &str) -> bool {
        self.channels_now()
            .iter()
            .any(|c| c.id == id && c.channel_type == Some(ChannelType::Session))
    }

    /// `grantSessionAccess`: each person is a grant to the session itself,
    /// never to the destination or a parent.
    async fn grant_session_access(
        &self,
        source: &str,
        people: &[String],
        attempt: &ShareAttempt,
        include_agents: bool,
        participant: bool,
        confirmed: &mut usize,
    ) -> Result<(), String> {
        let distinct: HashSet<&String> = people.iter().collect();
        if people.len() > MAX_PEOPLE || distinct.len() != people.len() {
            return Err("Choose at most 100 distinct recipients.".into());
        }
        for key in people {
            if participant {
                self.add_session_participant(source, key, include_agents)
                    .await?;
            } else {
                self.add_member(source, key).await?;
            }
            attempt.progress().granted.insert(key.clone());
            *confirmed += 1;
        }
        Ok(())
    }

    /// `destinationShareAudience`: one strong roster read of the
    /// destination, never a cached chooser list.
    async fn share_audience(
        &self,
        destination: &str,
        archived: &HashSet<String>,
    ) -> Result<Vec<String>, String> {
        let fresh = self.refresh_membership(destination).await.map_err(|_| {
            "Destination membership could not be confirmed. Retry to share.".to_owned()
        })?;
        audience_keys(&fresh, &self.viewer, archived)
    }

    /// `applySharedChannelDetails`: promotes the conversation in place
    /// through the details owner. A retry checks an uncertain save rather
    /// than publishing it again.
    async fn apply_shared_details(
        &self,
        id: &str,
        draft: &DetailsDraft,
        expected: &DetailsDraft,
    ) -> Result<(), String> {
        if let Some(pending) = self.details_attempt(id) {
            if pending.draft != *draft || pending.status != AttemptStatus::Unconfirmed {
                return Err("Finish the existing channel details change before sharing.".into());
            }
            self.check_channel_details(id).await?;
        }
        let base = self.channel_details(id).await?;
        if !base.can_edit {
            return Err("You no longer have permission to share this conversation.".into());
        }
        if base.draft != *draft {
            if base.draft != *expected {
                return Err(
                    "Channel settings changed after sharing. Check them in Messages before continuing."
                        .into(),
                );
            }
            self.save_channel_details(id, base.version, draft.clone())
                .await?;
        }
        Ok(())
    }

    /// `publishSessionLink`: rechecks both channels, then posts the link
    /// through the outbox with recovery, so a retry continues the same
    /// message, and waits for its receipt.
    async fn publish_session_link(
        &self,
        source: &str,
        destination: &str,
        previous: Option<EventId>,
        on_queued: impl FnOnce(EventId),
    ) -> Result<EventId, String> {
        let viewer = self.viewer.clone();
        let fresh_source = self.refresh_membership(source).await?;
        let fresh_destination = self.refresh_membership(destination).await?;
        if !eligible(&fresh_source, &viewer, true) || !eligible(&fresh_destination, &viewer, false)
        {
            return Err(ACCESS_CHANGED.into());
        }
        let outbox: Outbox = self
            .outbox()
            .await
            .ok_or("Message posting is unavailable.")?;
        outbox.ready().await.map_err(|error| error.to_string())?;
        let listed = self.channels_now();
        let now = |id: &str, fallback: &ChannelSummary| {
            listed
                .iter()
                .find(|c| c.id == id)
                .cloned()
                .unwrap_or_else(|| fallback.clone())
        };
        if !eligible(&now(source, &fresh_source), &viewer, true)
            || !eligible(&now(destination, &fresh_destination), &viewer, false)
        {
            return Err(ACCESS_CHANGED.into());
        }
        let existing = previous.and_then(|id| outbox.get(&id));
        if let (Some(previous), None) = (previous, &existing) {
            return if self.link_delivered(previous, source, destination).await? {
                Ok(previous)
            } else {
                Err(
                    "Link delivery is unconfirmed. Check Outbox and the channel before posting again."
                        .into(),
                )
            };
        }
        if existing
            .as_ref()
            .is_some_and(|op| !is_link(op, &viewer, source, destination))
        {
            return Err(
                "Saved link does not match this share. Check Outbox before retrying.".into(),
            );
        }
        if let Some(op) = &existing
            && matches!(op.delivery, Delivery::Accepted | Delivery::Seen)
        {
            outbox
                .acknowledge(&op.id())
                .await
                .map_err(|error| error.to_string())?;
            return Ok(op.id());
        }
        let recovery = link_recovery(source, destination);
        // An operation saved before this share owned it may still be in
        // flight: protect its exact id before an echo retires it.
        if let Some(op) = existing.as_ref().filter(|op| op.recovery.is_none()) {
            outbox
                .recover(&op.id(), recovery.clone())
                .await
                .map_err(|error| error.to_string())?;
        }
        let id = match &existing {
            Some(op) => op.id(),
            None => self.send_link(source, destination, recovery).await?,
        };
        on_queued(id);
        if existing
            .as_ref()
            .is_some_and(|op| matches!(op.delivery, Delivery::Failed | Delivery::Unknown))
        {
            outbox.retry(&id, None).map_err(|error| error.to_string())?;
        }
        let waiting = outbox.clone();
        self.runtime
            .spawn(async move {
                tokio::time::timeout(LINK_TIMEOUT, link_receipt(&waiting, id))
                    .await
                    .unwrap_or_else(|_| Err(LINK_UNCONFIRMED.into()))
            })
            .await
            .unwrap_or_else(|_| Err(LINK_UNCONFIRMED.into()))?;
        outbox
            .acknowledge(&id)
            .await
            .map_err(|error| error.to_string())?;
        Ok(id)
    }

    /// Queues the link message with its recovery.
    async fn send_link(
        &self,
        source: &str,
        destination: &str,
        recovery: Recovery,
    ) -> Result<EventId, String> {
        let (content, destination) = (session_link_message(source), destination.to_owned());
        self.ops()
            .with(move |actor| {
                let outbox = actor
                    .outbox
                    .clone()
                    .ok_or("Message posting is unavailable.")?;
                if !actor
                    .discovery
                    .as_ref()
                    .is_some_and(|d| d.can_participate(&destination))
                {
                    return Err(ACCESS_CHANGED.to_owned());
                }
                let emoji = actor.emoji.snapshot();
                let draft = messages::message(&Compose {
                    channel_id: &destination,
                    content: &content,
                    mentions: &Mentions::default(),
                    attachments: &[],
                    emoji: emoji.catalog(),
                    origin: &actor.origin,
                })
                .map_err(|error| error.to_string())?;
                outbox
                    .send(
                        draft,
                        SendOptions {
                            recovery: Some(recovery),
                            gate: None,
                        },
                    )
                    .map_err(|error| error.to_string())
            })
            .await?
    }

    /// Whether a link no longer in the outbox reached the relay.
    async fn link_delivered(
        &self,
        id: EventId,
        source: &str,
        destination: &str,
    ) -> Result<bool, String> {
        let ctx = self.ops().context().await?;
        let filter = Filter::from_iter([
            ("ids".to_owned(), json!([id.to_hex()])),
            ("limit".to_owned(), json!(1)),
            ("consistency".to_owned(), json!("strong")),
        ]);
        let (viewer, content) = (self.viewer.clone(), session_link_message(source));
        let destination = destination.to_owned();
        self.run(async move {
            let events = ctx
                .reader
                .read(
                    vec![filter],
                    ReadOptions {
                        fresh: true,
                        ..ReadOptions::default()
                    },
                )
                .await
                .map_err(|error| error.message)?;
            Ok(events.iter().any(|event| {
                event.id_hex() == id.to_hex()
                    && event.kind_u16() == kinds::CHAT
                    && event.pubkey_hex() == viewer
                    && event.event().content == content
                    && event.has_tag("h", &destination)
            }))
        })
        .await
    }
}

/// Waits until the outbox shows the link accepted or seen. Its recovery
/// keeps it listed after an echo.
async fn link_receipt(outbox: &Outbox, id: EventId) -> Result<(), String> {
    let wait = Wait {
        missing: Some("Link delivery is unconfirmed. Check the channel before retrying."),
        failed: "Link delivery is unconfirmed. Retry the saved message.",
        unknown_fails: true,
        closed: LINK_UNCONFIRMED,
    };
    delivered(outbox, id, &wait).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use nostr::prelude::{Keys, Kind, Tag, Timestamp, UnsignedEvent};

    const SOURCE: &str = "11111111-1111-4111-8111-111111111111";
    const DEST: &str = "22222222-2222-4222-8222-222222222222";

    fn intent(destination: Destination, audience: Audience) -> ShareIntent {
        ShareIntent {
            destination,
            audience,
            name: None,
            private: None,
            details: None,
            details_base: None,
            channel_people: Vec::new(),
            session_people: Vec::new(),
        }
    }

    fn op(content: &str, channel: &str, recovery: Option<Recovery>) -> Operation {
        let keys = Keys::generate();
        let event = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(1),
            Kind::from(kinds::CHAT),
            vec![Tag::parse(["h", channel]).unwrap()],
            content,
        );
        Operation {
            event,
            signed: None,
            recovery,
            acknowledged: false,
            guarded: false,
            delivery: Delivery::Sending,
            error: None,
        }
    }

    #[test]
    fn formats_the_link_message() {
        assert_eq!(
            session_link_message(SOURCE),
            format!("[Session · 11111111](buzz://channel/{SOURCE})")
        );
        assert_eq!(session_link("a b/c"), "buzz://channel/a%20b%2Fc");
        assert_eq!(session_link("é"), "buzz://channel/%C3%A9");
    }

    #[test]
    fn matches_only_the_exact_link_receipt() {
        let recovery = link_recovery(SOURCE, DEST);
        assert_eq!(recovery.key, format!("session-share-link:{SOURCE}:{DEST}"));
        let link = session_link_message(SOURCE);
        assert!(is_session_link_receipt(&op(
            &link,
            DEST,
            Some(recovery.clone())
        )));
        // No recovery, another channel, other content or value.
        assert!(!is_session_link_receipt(&op(&link, DEST, None)));
        assert!(!is_session_link_receipt(&op(
            &link,
            SOURCE,
            Some(recovery.clone())
        )));
        assert!(!is_session_link_receipt(&op(
            "hello",
            DEST,
            Some(recovery.clone())
        )));
        let other = Recovery {
            value: "2".into(),
            ..recovery.clone()
        };
        assert!(!is_session_link_receipt(&op(&link, DEST, Some(other))));
        let malformed = Recovery {
            key: format!("session-share-link:{SOURCE}:{DEST}:x"),
            value: "1".into(),
        };
        assert!(!is_session_link_receipt(&op(&link, DEST, Some(malformed))));
        let member = Recovery {
            key: format!("member-add:{DEST}:{SOURCE}"),
            value: "1".into(),
        };
        assert!(!is_session_link_receipt(&op(&link, DEST, Some(member))));
    }

    #[test]
    fn plans_a_direct_share_in_place() {
        let direct = intent(Destination::Channel(SOURCE.into()), Audience::Selected);
        assert!(direct.is_direct(SOURCE));
        assert_eq!(
            steps(&direct, SOURCE, false),
            [Step::ConfirmAccess, Step::SaveDetails, Step::MoveToMessages]
        );
    }

    #[test]
    fn plans_a_new_channel_share() {
        let mut new = intent(Destination::New, Audience::Everyone);
        new.channel_people = vec!["a".into(), "b".into()];
        assert_eq!(
            steps(&new, SOURCE, false),
            [
                Step::CreateChannel,
                Step::AddToNewChannel("a".into()),
                Step::AddToNewChannel("b".into()),
                Step::CheckMembers,
                Step::ConfirmAccess,
                Step::PostLink,
            ]
        );
        // A retry keeps the frozen roster: no second read.
        // It also creates the channel under the id chosen first.
        let attempt = ShareAttempt {
            intent: new.clone(),
            progress: Mutex::default(),
        };
        assert_eq!(attempt.creating(), attempt.creating());
        assert!(!steps(&new, SOURCE, true).contains(&Step::CheckMembers));
        let selected = intent(Destination::Channel(DEST.into()), Audience::Selected);
        assert_eq!(
            steps(&selected, SOURCE, false),
            [Step::ConfirmAccess, Step::PostLink]
        );
    }

    #[test]
    fn labels_each_step() {
        let name = |key: &str| format!("<{key}>");
        assert_eq!(Step::CreateChannel.label(name), "Creating channel…");
        assert_eq!(
            Step::AddToNewChannel("k".into()).label(name),
            "Adding <k> to the new channel…"
        );
        assert_eq!(Step::CheckMembers.label(name), "Checking channel members…");
        assert_eq!(
            Step::ConfirmAccess.label(name),
            "Confirming session access…"
        );
        assert_eq!(Step::SaveDetails.label(name), "Saving channel settings…");
        assert_eq!(Step::MoveToMessages.label(name), "Moving to Messages…");
        assert_eq!(Step::PostLink.label(name), "Posting session link…");
    }

    #[test]
    fn reports_what_already_took_effect() {
        assert_eq!(failure_text("Failed.", false, false, 0), "Failed.");
        assert_eq!(
            failure_text("Failed.", true, true, 1),
            "Failed. Channel settings were saved; retry to finish moving to Messages. Conversation is in Messages; access already granted is not revoked. 1 session participant confirmed."
        );
        assert_eq!(
            failure_text("Failed.", false, false, 3),
            "Failed. 3 session participants confirmed."
        );
    }

    fn channel(members: Vec<String>) -> ChannelSummary {
        ChannelSummary {
            id: DEST.into(),
            channel_type: Some(ChannelType::Stream),
            members,
            ..Default::default()
        }
    }

    #[test]
    fn freezes_the_destination_audience() {
        let viewer = "a".repeat(64);
        let (b, c) = ("b".repeat(64), "c".repeat(64));
        let none = HashSet::new();
        let roster = channel(vec![viewer.clone(), b.clone(), b.clone(), c.clone()]);
        assert_eq!(
            audience_keys(&roster, &viewer, &none).unwrap(),
            [b.clone(), c.clone()]
        );
        // The viewer must belong, in a writable stream or forum.
        assert!(audience_keys(&channel(vec![b.clone()]), &viewer, &none).is_err());
        let mut session = roster.clone();
        session.channel_type = Some(ChannelType::Session);
        assert!(audience_keys(&session, &viewer, &none).is_err());
        let mut read_only = roster.clone();
        read_only.read_only = true;
        assert!(audience_keys(&read_only, &viewer, &none).is_err());
        // Archived members and oversized rosters need selected people.
        let archived: HashSet<String> = [c.clone()].into();
        assert_eq!(
            audience_keys(&roster, &viewer, &archived).unwrap_err(),
            "A destination member is archived. Choose selected people instead."
        );
        let crowd: Vec<String> = std::iter::once(viewer.clone())
            .chain((0..101).map(|n| format!("{n:064x}")))
            .collect();
        assert_eq!(
            audience_keys(&channel(crowd), &viewer, &none).unwrap_err(),
            "This channel has more than 100 recipients. Choose selected people instead."
        );
        let invalid = channel(vec![viewer.clone(), "nope".into()]);
        assert_eq!(
            audience_keys(&invalid, &viewer, &none).unwrap_err(),
            "Destination membership contains an invalid identity."
        );
    }
}
