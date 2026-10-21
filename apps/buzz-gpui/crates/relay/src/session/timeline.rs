//! One channel's window: its read history plus live traffic, folded into
//! rows (`store.ts` `WindowState`).

use super::channels::Head;
use crate::event::{Verified, kinds};
use crate::fold::{FoldOptions, LocalDelivery, MessageClock, MessageProjection, MessageRow};
use crate::window::Cursor;
use nostr::prelude::{EventId, PublicKey};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::watch;

/// Rows a window keeps before older pages are refused.
pub const MAX_ROWS: usize = 2400;
pub const MAX_BYTES: usize = 8 << 20;
/// A head newer than this is reused without a read.
pub const FRESH_FOR_MS: u64 = 60_000;

/// Kinds a channel window takes from traffic (`store.ts` `accept`).
const WINDOW_KINDS: &[u16] = &[
    kinds::CHAT,
    kinds::AGENT_MESSAGE,
    kinds::DIFF,
    kinds::SYSTEM_MESSAGE,
    kinds::EDIT,
    kinds::DELETION,
    kinds::MODERATOR_DELETE,
    kinds::REACTION,
    kinds::THREAD_SUMMARY,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineStatus {
    Loading,
    Ready,
    Error(String),
}

/// What a channel view renders.
#[derive(Clone, Debug)]
pub struct TimelineSnapshot {
    pub channel: String,
    pub status: TimelineStatus,
    pub rows: Arc<[Arc<MessageRow>]>,
    pub has_more: bool,
    pub loading_older: bool,
    /// Older history exists but is over the retention budget.
    pub history_limited: bool,
    /// Shown from before a disconnect, or from disk: not yet confirmed
    /// (`freshness: "cached"`).
    pub stale: bool,
    /// The rows came from this device's saved copy and no relay read has
    /// replaced them yet.
    pub restored: bool,
    pub error: Option<String>,
}

impl TimelineSnapshot {
    pub fn loading(channel: &str) -> Self {
        Self {
            channel: channel.to_string(),
            status: TimelineStatus::Loading,
            rows: Arc::new([]),
            has_more: false,
            loading_older: false,
            history_limited: false,
            stale: false,
            restored: false,
            error: None,
        }
    }
}

/// A window's own state, as `ChannelWindow.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WindowStatus {
    Idle,
    Loading,
    Ready,
    Error,
}

pub(super) struct Timeline {
    pub id: String,
    pub viewers: usize,
    /// From the session's counter; a read started under another
    /// generation is dropped (eviction, reset, revocation).
    pub generation: u64,
    /// Read-owned history: the head and older pages.
    events: HashMap<EventId, Verified>,
    /// Live and finite-read traffic that belongs here.
    traffic: HashMap<EventId, Verified>,
    /// Our own outbox operations for this channel, with their state.
    local: Vec<(Verified, LocalDelivery)>,
    projection: MessageProjection,
    pub cursor: Option<Cursor>,
    pub has_more: bool,
    /// Only the head is loaded, so a catch-up may move the cursor.
    pub at_head: bool,
    pub status: WindowStatus,
    /// A head read for this window is in flight.
    pub loading_head: bool,
    /// The channel's 39001 was read for this window since it opened or
    /// its access last changed.
    pub admins_read: bool,
    pub loading_older: bool,
    pub history_limited: bool,
    pub stale: bool,
    pub restored: bool,
    pub error: Option<String>,
    current: Arc<TimelineSnapshot>,
    /// One per open view; closed ones are dropped on the next publish.
    views: Vec<watch::Sender<Arc<TimelineSnapshot>>>,
}

impl Timeline {
    pub fn new(
        id: String,
        relay_author: PublicKey,
        clock: Arc<MessageClock>,
        generation: u64,
        session: bool,
    ) -> Self {
        let projection = MessageProjection::new(
            id.clone(),
            relay_author,
            FoldOptions {
                signing_authority: Some(relay_author),
                include_replies: session,
                ..Default::default()
            },
        )
        .with_clock(clock);
        let snapshot = Arc::new(TimelineSnapshot::loading(&id));
        Self {
            id,
            viewers: 0,
            generation,
            events: HashMap::new(),
            traffic: HashMap::new(),
            local: Vec::new(),
            projection,
            cursor: None,
            has_more: true,
            at_head: true,
            status: WindowStatus::Idle,
            loading_head: false,
            admins_read: false,
            loading_older: false,
            history_limited: false,
            stale: false,
            restored: false,
            error: None,
            current: snapshot,
            views: Vec::new(),
        }
    }

    /// Starts feeding a view, beginning with the current snapshot.
    pub fn attach(&mut self, view: watch::Sender<Arc<TimelineSnapshot>>) {
        view.send_replace(self.current.clone());
        self.views.push(view);
    }

    pub fn idle(&self) -> bool {
        matches!(self.status, WindowStatus::Idle | WindowStatus::Error)
    }

    /// Session channels keep replies as rows; discovery may learn that a
    /// channel is one after its window opened.
    pub fn set_session(&mut self, session: bool) {
        let options = FoldOptions {
            include_replies: session,
            ..self.projection.options().clone()
        };
        self.projection.set_options(options);
    }

    /// The channel's admins from its 39001, whose moderator deletes apply.
    pub fn set_admins(&mut self, admins: &HashSet<PublicKey>) {
        if self.projection.options().channel_admins == *admins {
            return;
        }
        let options = FoldOptions {
            channel_admins: admins.clone(),
            ..self.projection.options().clone()
        };
        self.projection.set_options(options);
    }

    /// Seeds the window with live events retained before it opened
    /// (`tails`).
    pub fn seed_traffic(&mut self, events: &[Verified]) {
        for event in events {
            self.traffic.insert(event.id, event.clone());
        }
    }

    /// Shows a head: its events replace the read history (`patchFromHead`).
    pub fn apply_head(&mut self, head: &Head) {
        self.events = head.events.iter().map(|e| (e.id, e.clone())).collect();
        self.cursor = head.cursor.clone();
        self.has_more = head.has_more;
        self.at_head = true;
        self.restored = head.restored;
        self.status = WindowStatus::Ready;
        self.loading_older = false;
        self.error = None;
        self.stale = head.cached;
        self.history_limited = false;
    }

    /// An older page: added to the history unless the result would be over
    /// the budget, which stops paging back instead.
    pub fn apply_older(&mut self, events: Vec<Verified>, cursor: Option<Cursor>, has_more: bool) {
        self.loading_older = false;
        self.error = None;
        let mut rows = self.row_count();
        let mut bytes: usize = self.events.values().map(Verified::byte_size).sum::<usize>() + 2;
        for event in &events {
            if self.events.contains_key(&event.id) {
                continue;
            }
            rows += usize::from(kinds::CHANNEL_ROW_KINDS.contains(&event.kind_u16()));
            bytes += event.byte_size() + 1;
        }
        if rows > MAX_ROWS || bytes > MAX_BYTES {
            self.history_limited = true;
            return;
        }
        for event in events {
            self.events.insert(event.id, event);
        }
        self.at_head = false;
        self.cursor = cursor;
        self.has_more = has_more && self.cursor.is_some();
        self.restored = false;
        self.stale = false;
        self.status = WindowStatus::Ready;
    }

    /// A catch-up head merged into the window (`catchUp`): the cursor only
    /// moves while nothing older than the head is loaded.
    pub fn caught_up(&mut self, head: &Head) {
        self.restored = false;
        if self.at_head {
            self.cursor = head.cursor.clone();
            self.has_more = head.has_more;
        }
        self.status = WindowStatus::Ready;
        self.stale = false;
        self.error = None;
    }

    /// A read failed: rows already shown stay, marked unconfirmed.
    pub fn read_failed(&mut self, error: String, older: bool) {
        self.loading_older = false;
        self.error = Some(error);
        if older || self.has_rows() {
            self.stale = true;
            if self.status != WindowStatus::Ready {
                self.status = WindowStatus::Ready;
            }
        } else {
            self.status = WindowStatus::Error;
        }
    }

    /// Access to the channel is gone: nothing retained stays readable.
    pub fn deny(&mut self, error: String, generation: u64) {
        self.generation = generation;
        self.events.clear();
        self.traffic.clear();
        self.loading_head = false;
        self.admins_read = false;
        self.loading_older = false;
        self.status = WindowStatus::Error;
        self.error = Some(error);
    }

    /// Re-filters what is retained after an access change.
    pub fn purge(&mut self, visible: impl Fn(&[Verified]) -> Vec<Verified>, generation: u64) {
        self.generation = generation;
        self.loading_head = false;
        self.admins_read = false;
        self.loading_older = false;
        let all: Vec<Verified> = self
            .events
            .values()
            .chain(self.traffic.values())
            .cloned()
            .collect();
        self.events = visible(&all).into_iter().map(|e| (e.id, e)).collect();
        self.traffic.clear();
        self.status = if self.events.is_empty() {
            WindowStatus::Idle
        } else {
            WindowStatus::Ready
        };
    }

    pub fn has_rows(&self) -> bool {
        !self.current.rows.is_empty()
    }

    /// Takes events that belong here: rows and overlays tagged with this
    /// channel, and anything referring to an event already here
    /// (transitively, so a deletion of a reaction follows the reaction).
    /// Over the budget, only the newest rows and their overlays stay.
    pub fn accept(&mut self, events: &[Verified], local: &HashSet<EventId>) -> bool {
        let mut known: HashSet<EventId> = self
            .events
            .keys()
            .chain(self.traffic.keys())
            .copied()
            .collect();
        let mut pending: Vec<&Verified> = events
            .iter()
            .filter(|event| WINDOW_KINDS.contains(&event.kind_u16()) && !known.contains(&event.id))
            .collect();
        let mut incoming = Vec::new();
        loop {
            let before = pending.len();
            pending.retain(|event| {
                let belongs = event.has_tag("h", &self.id)
                    || event.tags_named("e").any(|tag| {
                        tag.get(1)
                            .and_then(|id| EventId::from_hex(id).ok())
                            .is_some_and(|id| known.contains(&id))
                    });
                if belongs {
                    known.insert(event.id);
                    incoming.push((*event).clone());
                }
                !belongs
            });
            if pending.len() == before {
                break;
            }
        }
        if incoming.is_empty() {
            return false;
        }
        for event in incoming {
            self.traffic.insert(event.id, event);
        }
        self.trim(local);
        true
    }

    /// Keeps a long-open window within its budget (`accept` overflow):
    /// the newest rows plus local ones, their overlays, then the oldest
    /// quarter at a time until the bytes fit.
    fn trim(&mut self, local: &HashSet<EventId>) {
        let counted = |event: &Verified| {
            kinds::CHANNEL_ROW_KINDS.contains(&event.kind_u16()) && !local.contains(&event.id)
        };
        let all: Vec<&Verified> = self.events.values().chain(self.traffic.values()).collect();
        let rows = all.iter().filter(|event| counted(event)).count();
        let bytes: usize = all.iter().map(|event| event.byte_size() + 1).sum::<usize>() + 1;
        if rows <= MAX_ROWS && bytes <= MAX_BYTES {
            return;
        }
        let mut newest: Vec<&Verified> = all.iter().copied().filter(|e| counted(e)).collect();
        newest.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut keep: HashSet<EventId> = newest
            .into_iter()
            .take(MAX_ROWS)
            .map(|event| event.id)
            .collect();
        keep.extend(local.iter().copied());
        let mut retained: Vec<Verified> = all
            .into_iter()
            .filter(|event| {
                keep.contains(&event.id)
                    || (!kinds::CHANNEL_ROW_KINDS.contains(&event.kind_u16())
                        && event.tags_named("e").any(|tag| {
                            tag.get(1)
                                .and_then(|id| EventId::from_hex(id).ok())
                                .is_some_and(|id| keep.contains(&id))
                        }))
            })
            .cloned()
            .collect();
        retained.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        let mut bytes: usize = retained.iter().map(|e| e.byte_size() + 1).sum::<usize>() + 1;
        while !retained.is_empty() && bytes > MAX_BYTES {
            let shed = retained.len().div_ceil(4).max(1);
            for event in retained.drain(..shed) {
                bytes -= event.byte_size() + 1;
            }
        }
        let ids: HashSet<EventId> = retained.iter().map(|event| event.id).collect();
        self.events.retain(|id, _| ids.contains(id));
        self.traffic.retain(|id, _| ids.contains(id));
        self.history_limited = true;
    }

    /// Replaces the local operations shown here. Content stays visible when
    /// it failed so it can be retried; a failed edit or reaction does not.
    pub fn set_local(&mut self, operations: &[(Verified, LocalDelivery)]) {
        let mut known: HashSet<EventId> = self
            .events
            .keys()
            .chain(self.traffic.keys())
            .copied()
            .collect();
        for (event, _) in operations {
            if kinds::CHANNEL_ROW_KINDS.contains(&event.kind_u16()) && event.has_tag("h", &self.id)
            {
                known.insert(event.id);
            }
        }
        let mut local: Vec<(Verified, LocalDelivery)> = operations
            .iter()
            .filter(|(event, delivery)| {
                let content = kinds::CHANNEL_ROW_KINDS.contains(&event.kind_u16());
                let here = event.has_tag("h", &self.id)
                    || event.tags_named("e").any(|tag| {
                        tag.get(1)
                            .and_then(|id| EventId::from_hex(id).ok())
                            .is_some_and(|id| known.contains(&id))
                    });
                here && (content || delivery.status != crate::fold::Delivery::Failed)
            })
            .cloned()
            .collect();
        local.sort_by_key(|(event, _)| event.id);
        self.local = local;
    }

    /// Every event retained here, to seed a thread view.
    pub fn events(&self) -> Vec<Verified> {
        self.events
            .values()
            .chain(self.traffic.values())
            .cloned()
            .collect()
    }

    /// An event retained here (`retainedEvent`), for edits, reactions,
    /// removals and visibility evidence.
    pub fn find(&self, id: &EventId) -> Option<Verified> {
        self.events
            .get(id)
            .or_else(|| self.traffic.get(id))
            .or_else(|| {
                self.local
                    .iter()
                    .find(|(event, _)| event.id == *id)
                    .map(|(event, _)| event)
            })
            .cloned()
    }

    /// The folded row shown for `id`.
    pub fn row(&self, id: &EventId) -> Option<Arc<MessageRow>> {
        self.current.rows.iter().find(|row| row.id == *id).cloned()
    }

    /// The newest verified top-level message: the bottom of the channel, as
    /// a reading catch-up sees it. Unsent local rows do not count.
    pub fn bottom(&self) -> Option<String> {
        self.events
            .values()
            .chain(self.traffic.values())
            .filter(|event| {
                crate::unread::CONTENT_KINDS.contains(&event.kind_u16())
                    && crate::unread::thread_reference(event).is_none()
            })
            .max_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| b.id.cmp(&a.id))
            })
            .map(Verified::id_hex)
    }

    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// Ids of everyone the rows show (`rowProfileIds`), for profile reads:
    /// membership actors and targets, authors, workflow owners, mentions
    /// and mention references, participants and reaction authors.
    pub fn people(&self) -> Vec<String> {
        rows_people(&self.projection.snapshot())
    }

    /// The sidebar preview of what is shown here.
    pub fn preview(&self) -> Option<String> {
        message_preview(&self.current.rows)
    }

    /// Refolds and publishes if anything visible changed.
    pub fn publish(&mut self) {
        let combined = self
            .events
            .values()
            .chain(self.traffic.values())
            .chain(self.local.iter().map(|(event, _)| event));
        let deliveries = self
            .local
            .iter()
            .map(|(event, delivery)| (event.id, delivery.clone()));
        let (rows, _) = self.projection.reconcile_local(combined, deliveries);
        let status = match (self.status, &self.error) {
            (WindowStatus::Error, Some(error)) => TimelineStatus::Error(error.clone()),
            (WindowStatus::Error, None) => {
                TimelineStatus::Error("Conversation is unavailable".into())
            }
            (WindowStatus::Ready, _) => TimelineStatus::Ready,
            (WindowStatus::Idle | WindowStatus::Loading, _) => TimelineStatus::Loading,
        };
        let next = TimelineSnapshot {
            channel: self.id.clone(),
            status,
            rows,
            has_more: self.has_more,
            loading_older: self.loading_older,
            history_limited: self.history_limited,
            stale: self.stale,
            restored: self.restored,
            error: self.error.clone(),
        };
        let current = &self.current;
        let same = Arc::ptr_eq(&current.rows, &next.rows)
            && current.status == next.status
            && current.has_more == next.has_more
            && current.loading_older == next.loading_older
            && current.history_limited == next.history_limited
            && current.stale == next.stale
            && current.restored == next.restored
            && current.error == next.error;
        if same {
            return;
        }
        self.current = Arc::new(next);
        self.views.retain(|view| !view.is_closed());
        for view in &self.views {
            view.send_replace(self.current.clone());
        }
    }

    fn row_count(&self) -> usize {
        self.events
            .values()
            .filter(|event| kinds::CHANNEL_ROW_KINDS.contains(&event.kind_u16()))
            .count()
    }
}

/// Everyone some row shows, for profile reads.
pub(super) fn rows_people(rows: &[Arc<MessageRow>]) -> Vec<String> {
    let mut people = Vec::new();
    for row in rows {
        if let Some(membership) = &row.membership {
            people.push(membership.actor.to_hex());
            people.push(membership.target.to_hex());
            continue;
        }
        people.push(row.author.to_hex());
        people.extend(row.workflow_owner.iter().map(|p| p.to_hex()));
        people.extend(row.mentions.iter().map(|p| p.to_hex()));
        people.extend(row.mention_references.iter().map(|p| p.to_hex()));
        people.extend(row.participants.iter().map(|p| p.to_hex()));
        for group in &row.reactions {
            people.extend(group.events.iter().map(|r| r.author.to_hex()));
        }
    }
    people
}

/// The newest conversational row as a sidebar preview (`messagePreview`):
/// membership activity never replaces it.
pub(super) fn message_preview(rows: &[Arc<MessageRow>]) -> Option<String> {
    rows.iter()
        .rev()
        .find(|row| row.membership.is_none())
        .map(|row| match &row.diff {
            Some(diff) => format!(
                "Diff: {}{}",
                diff.file_path
                    .as_deref()
                    .filter(|path| !path.is_empty())
                    .unwrap_or("patch"),
                diff.description
                    .as_deref()
                    .filter(|description| !description.is_empty())
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default()
            ),
            None => row.content.clone(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{CHANNEL, Signer, T0};

    fn window(signer: &Signer) -> Timeline {
        Timeline::new(
            CHANNEL.into(),
            signer.relay.public_key(),
            Arc::default(),
            1,
            false,
        )
    }

    fn message(signer: &Signer, at: u64, content: &str) -> Verified {
        signer.sign(&signer.viewer, kinds::CHAT, at, content, &[&["h", CHANNEL]])
    }

    fn head(events: Vec<Verified>, cached: bool) -> Head {
        Head {
            events: events.into(),
            cursor: None,
            has_more: false,
            saved_at: 0,
            cached,
            restored: cached,
            preview: None,
            people: Vec::new(),
        }
    }

    #[test]
    fn a_head_seeds_the_window_and_marks_saved_rows() {
        let signer = Signer::new();
        let mut timeline = window(&signer);
        assert!(timeline.idle());
        timeline.apply_head(&head(vec![message(&signer, T0, "saved")], true));
        timeline.publish();
        let snapshot = timeline.current.clone();
        assert_eq!(snapshot.status, TimelineStatus::Ready);
        assert!(snapshot.stale && snapshot.restored);
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(timeline.preview().as_deref(), Some("saved"));
    }

    #[test]
    fn multi_channel_events_belong_to_every_channel_they_name() {
        let signer = Signer::new();
        let mut timeline = window(&signer);
        let both = signer.sign(
            &signer.viewer,
            kinds::CHAT,
            T0,
            "both",
            &[&["h", "other"], &["h", CHANNEL]],
        );
        let elsewhere = signer.sign(&signer.viewer, kinds::CHAT, T0, "x", &[&["h", "other"]]);
        assert!(timeline.accept(&[both, elsewhere], &HashSet::new()));
        assert_eq!(timeline.events().len(), 1);
    }

    #[test]
    fn an_older_page_over_the_budget_is_not_kept() {
        let signer = Signer::new();
        let mut timeline = window(&signer);
        let page: Vec<Verified> = (0..MAX_ROWS as u64)
            .map(|n| message(&signer, T0 + n, "m"))
            .collect();
        timeline.apply_head(&head(page, false));
        let older = vec![message(&signer, T0 - 10, "old")];
        timeline.apply_older(
            older,
            Some(Cursor {
                created_at: T0 - 10,
                id: "0".repeat(64),
            }),
            true,
        );
        assert!(timeline.history_limited);
        assert_eq!(timeline.events().len(), MAX_ROWS, "the page never went in");
        assert!(timeline.at_head, "the cursor did not move");
    }

    #[test]
    fn live_overflow_keeps_the_newest_rows_their_overlays_and_local_sends() {
        let signer = Signer::new();
        let mut timeline = window(&signer);
        let oldest = message(&signer, T0, "oldest");
        let reaction = signer.sign(
            &signer.viewer,
            kinds::REACTION,
            T0 + 1,
            "+",
            &[&["e", &oldest.id_hex()]],
        );
        let local = message(&signer, T0 - 5, "mine, unsent");
        let mut events = vec![oldest.clone(), reaction.clone(), local.clone()];
        events.extend((1..=MAX_ROWS as u64).map(|n| message(&signer, T0 + 10 + n, "new")));
        let ids: HashSet<EventId> = [local.id].into();
        timeline.accept(&events, &ids);
        assert!(timeline.history_limited);
        let kept: HashSet<EventId> = timeline.events().iter().map(|e| e.id).collect();
        assert!(!kept.contains(&oldest.id), "the oldest row went");
        assert!(!kept.contains(&reaction.id), "with its overlay");
        assert!(kept.contains(&local.id), "local sends stay");
        assert_eq!(kept.len(), MAX_ROWS + 1);
    }

    #[test]
    fn a_live_moderator_delete_hides_its_target_only_when_signed_by_an_admin() {
        let signer = Signer::new();
        let admin = nostr::prelude::Keys::generate();
        let stranger = nostr::prelude::Keys::generate();
        let mut timeline = window(&signer);
        timeline.set_admins(&[admin.public_key()].into());
        let kept = message(&signer, T0, "kept");
        let removed = message(&signer, T0 + 1, "removed");
        timeline.apply_head(&head(vec![kept.clone(), removed.clone()], false));
        timeline.publish();
        let delete = |keys: &nostr::prelude::Keys, target: &Verified| {
            let target = target.id_hex();
            signer.sign(
                keys,
                kinds::MODERATOR_DELETE,
                T0 + 2,
                "",
                &[&["h", CHANNEL], &["e", &target]],
            )
        };
        let live = [delete(&stranger, &kept), delete(&admin, &removed)];
        assert!(timeline.accept(&live, &HashSet::new()));
        timeline.publish();
        let ids: Vec<EventId> = timeline.current.rows.iter().map(|row| row.id).collect();
        assert_eq!(ids, [kept.id]);
        // Without the admin set the reference rule (author only) holds.
        timeline.set_admins(&HashSet::new());
        timeline.publish();
        assert_eq!(timeline.current.rows.len(), 2);
    }

    #[test]
    fn failed_reads_keep_rows_and_mark_them_unconfirmed() {
        let signer = Signer::new();
        let mut timeline = window(&signer);
        timeline.read_failed("Relay is unreachable".into(), false);
        timeline.publish();
        assert_eq!(
            timeline.current.status,
            TimelineStatus::Error("Relay is unreachable".into())
        );
        timeline.apply_head(&head(vec![message(&signer, T0, "hi")], false));
        timeline.publish();
        timeline.read_failed("Relay is unreachable".into(), false);
        timeline.publish();
        assert_eq!(timeline.current.status, TimelineStatus::Ready);
        assert!(timeline.current.stale);
        assert_eq!(timeline.current.rows.len(), 1);
    }

    #[test]
    fn previews_skip_membership_rows_and_describe_diffs() {
        let signer = Signer::new();
        let relay = signer.relay.public_key();
        let diff = signer.sign(
            &signer.viewer,
            kinds::DIFF,
            T0,
            "diff --git",
            &[
                &["h", CHANNEL],
                &["file", "src/main.rs"],
                &["description", "Fix it"],
            ],
        );
        let rows = crate::fold::fold_messages(CHANNEL, &relay, &[diff], &FoldOptions::default());
        let preview = message_preview(&rows).unwrap();
        assert!(preview.starts_with("Diff: "), "{preview}");
        assert_eq!(message_preview(&[]), None);
    }
}
