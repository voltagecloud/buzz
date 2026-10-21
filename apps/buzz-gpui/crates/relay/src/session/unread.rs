//! Read state and unread counts inside the session actor (spec §5i).
//!
//! [`ReadStateSync`] and [`Unread`] are synchronous owners; this module is
//! their I/O. It runs the marker refresh, the debounced publication, the
//! sampled evidence repair, the homes lookup and conversation lookups as
//! reads that report back as [`Message`]s, and turns the result into the
//! sidebar's [`UnreadCounts`].
//!
//! Reading: while a channel is selected and has an open [`ChannelView`],
//! the viewer may be reading it. Like the reference's dwell hook
//! (`use-reading.ts`), each report of what the view shows takes a reading
//! lease and, after [`READING_DWELL_MS`], catches up the channel bottom and
//! observes the visible rows, then ends the lease. Nothing is read until
//! the view reports what it shows; while it reports the bottom, new
//! top-level messages that arrive are read rather than counted. An open
//! [`ThreadView`] reads its thread the same way, with its own lease.
//!
//! [`ChannelView`]: super::ChannelView
//! [`ThreadView`]: super::ThreadView

use super::{Actor, Message};
use crate::ChannelType;
use crate::event::{Verified, Verifier};
use crate::http::{HttpError, RelayHttp};
use crate::read_state::{
    PublishCommand, PublishFailure, ReadCapability, ReadLookup, ReadMutationResult, ReadStateSync,
    ReadSyncSnapshot, ReadSyncStatus, ReadTarget, RefreshRequest, SnapshotDescriptor,
    TIMESTAMP_REFUSAL, parse_snapshot_response,
};
use crate::reader::{Events, Priority, ReadError, ReadOptions};
use crate::socket::Filter;
use crate::store::{Partition, SqliteReadJournal, SqliteThreadFollows};
use crate::unread::{
    CONTENT_KINDS, ChannelInfo, EvidenceRepair, Freshness, HomesRequest, LookupBatch, Manual,
    ReadingLease, RepliesPaging, ReplyTarget, ThreadActivitySnapshot, Unread, UnreadOptions,
    replies_paging, thread_reference,
};
use nostr::prelude::EventId;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

/// Every read-state and unread read gives up after this (`read-state.ts`,
/// `unread.ts`).
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long rows must stay in view before they count as read.
pub const READING_DWELL_MS: u64 = 300;
/// Most rows one viewport report may name.
const VIEW_IDS: usize = 128;

/// Unread state for the sidebar and the app badge. Channels with nothing
/// unread are absent; use [`UnreadCounts::channel`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnreadCounts {
    pub channels: HashMap<String, ChannelUnread>,
    /// Unread messages that mention the viewer, over every channel.
    pub mentions: u32,
    /// Unread direct messages, for the dock badge.
    pub direct: u32,
    /// The startup sample finished since the last (re)connect: counts
    /// cover every channel, not only the ones that loaded.
    pub observed: bool,
    /// Read-state sync health; `None` before the relay identified itself.
    pub sync: Option<ReadSyncSnapshot>,
    /// Newest message time per channel in Unix seconds, read or not, from
    /// the same evidence as the counts. For "recent" ordering.
    pub latest: HashMap<String, u64>,
    /// The newest message's text per channel, from the same evidence. For
    /// search's "Recent activity" detail.
    pub previews: HashMap<String, String>,
    /// Unread threads per channel (`unread.activity`), newest first, for
    /// every channel whose read state is known.
    pub activity: HashMap<String, Arc<ThreadActivitySnapshot>>,
}

impl UnreadCounts {
    pub fn channel(&self, id: &str) -> ChannelUnread {
        self.channels.get(id).copied().unwrap_or_default()
    }
}

/// One sidebar row (`UnreadBadge.tsx`). Counts are sampled ("observed"),
/// not exact, and are never capped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelUnread {
    /// Any unread row, or a manual "mark unread".
    pub unread: bool,
    /// Ordinary unread: bold label.
    pub bold: bool,
    /// Attention (mention, DM, followed thread, broadcast): accent dot.
    pub priority: bool,
    pub count: u32,
    pub attention: u32,
    /// Unread mentions outside DMs; a direct message is never a mention.
    pub mentions: u32,
    /// Marked unread by hand, here or on another device.
    pub manual: Manual,
    /// The counts are from before a disconnect.
    pub stale: bool,
    /// Threads with unread replies for the viewer (`activity().items`).
    pub threads: u32,
    pub threads_stale: bool,
}

/// One thread's unread state, for its summary in the timeline
/// (`useThreadUnread`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadUnread {
    pub manual: Manual,
    /// Observed unread replies; `None` without evidence.
    pub observed: Option<u32>,
    pub stale: bool,
}

/// What an [`UnreadWatch`] reports for the rows a view shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnreadDetail {
    /// Thread roots asked for, by root.
    pub threads: HashMap<EventId, ThreadUnread>,
    /// Messages asked for that are unread (`attention().unread`).
    pub unread: HashSet<EventId>,
    /// Messages asked for that the viewer marked unread for this visit.
    pub forced: HashSet<EventId>,
}

/// Follows the unread state of the rows one view shows. Dropping it ends
/// the watch.
pub struct UnreadWatch {
    pub snapshot: tokio::sync::watch::Receiver<Arc<UnreadDetail>>,
    pub(super) id: u64,
    pub(super) commands: tokio::sync::mpsc::UnboundedSender<Message>,
}

impl UnreadWatch {
    /// The thread roots and messages to report on, replacing the last set.
    pub fn set(&self, threads: Vec<EventId>, messages: Vec<EventId>) {
        let id = self.id;
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            if let Some(watch) = actor.watches.get_mut(&id) {
                watch.threads = threads;
                watch.messages = messages;
                watch.seen = None;
            }
            actor.publish_watches();
        })));
    }
}

impl Drop for UnreadWatch {
    fn drop(&mut self) {
        let id = self.id;
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor.watches.remove(&id);
        })));
    }
}

pub(super) struct WatchState {
    pub(super) channel: String,
    threads: Vec<EventId>,
    messages: Vec<EventId>,
    /// `(unread stamp, read-state changes)` behind the last report.
    seen: Option<(u64, u64)>,
    sender: tokio::sync::watch::Sender<Arc<UnreadDetail>>,
}

impl WatchState {
    pub(super) fn new(
        channel: String,
        sender: tokio::sync::watch::Sender<Arc<UnreadDetail>>,
    ) -> Self {
        Self {
            channel,
            threads: Vec::new(),
            messages: Vec::new(),
            seen: None,
            sender,
        }
    }
}

/// Explicit read actions from the UI.
pub(super) enum Mark {
    Channel(String),
    All,
    MessageRead(String, EventId),
    MessageUnread(String, EventId),
    /// The sidebar's "Mark as Unread": this device only.
    ChannelUnread(String),
    /// Clears a device-local "Mark as Unread" (`clearUnreadLocal`).
    ClearChannelUnread(String),
    /// A channel, thread or message marked unread on this device, or
    /// that mark cleared (`markUnreadLocal`, `clearUnreadLocal`).
    Local {
        target: crate::read_state::ReadTarget,
        clear: bool,
    },
    /// A channel or thread, read through one of its messages.
    Through {
        channel: String,
        root: Option<EventId>,
        message: EventId,
    },
}

struct RepairRun {
    repair: EvidenceRepair,
    next: usize,
    priority: Priority,
}

struct RepliesPage {
    targets: Vec<ReplyTarget>,
    until: Option<u64>,
    pages: u32,
}

struct LookupRun {
    batch: LookupBatch,
    fetched: Vec<Verified>,
    replies: Vec<Verified>,
    structure_read: bool,
    pages: VecDeque<RepliesPage>,
    current: Option<RepliesPage>,
}

/// The channel being read and what its view last reported.
struct Reading {
    channel: String,
    at_bottom: bool,
    visible: Vec<String>,
}

/// A thread panel being read: its visible replies, and the newest reply
/// when the panel sits at its bottom.
struct ThreadReading {
    view: u64,
    channel: String,
    visible: Vec<String>,
    bottom: Option<(String, String)>,
}

struct Dwell {
    lease: ReadingLease,
    ids: Vec<String>,
    bottom: bool,
    due: u64,
}

struct ThreadDwell {
    lease: ReadingLease,
    ids: Vec<String>,
    /// `(newest reply, root)` to catch the thread up through.
    bottom: Option<(String, String)>,
    due: u64,
}

pub(super) struct Reads {
    pub(super) sync: ReadStateSync<SqliteReadJournal>,
    pub(super) unread: Unread<SqliteThreadFollows>,
    descriptor: Option<SnapshotDescriptor>,
    /// Allowed channel ids and whether each is a DM, for the projection.
    channels: Vec<(String, bool)>,
    /// The marker refresh since the last (re)connect finished.
    refreshed: bool,
    roster_ready: bool,
    /// Live dropped; refresh again when it is back.
    reconnecting: bool,
    repair_wanted: bool,
    /// The next repair's read priority: foreground when asked for, in the
    /// background after a reconnect.
    repair_priority: Priority,
    repair: Option<RepairRun>,
    /// The startup sample finished since the last (re)connect.
    sampled: bool,
    lookup: Option<LookupRun>,
    /// Lookup backoff after a failure, in wall-clock ms.
    lookup_after: Option<u64>,
    reading: Option<Reading>,
    dwell: Option<Dwell>,
    thread_reading: Option<ThreadReading>,
    thread_dwell: Option<ThreadDwell>,
    /// `(unread stamp, read-state changes, sampled)` behind `counts`.
    seen: Option<(u64, u64, bool)>,
    counts: Arc<UnreadCounts>,
}

/// Wall-clock milliseconds: read-state versions are Unix seconds.
pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn type_name(channel_type: ChannelType) -> &'static str {
    match channel_type {
        ChannelType::Stream => "stream",
        ChannelType::Forum => "forum",
        ChannelType::Dm => "dm",
        ChannelType::Session => "session",
    }
}

/// `POST /query` with the exact complete-snapshot body (§5i.6).
async fn read_snapshot(
    http: &RelayHttp,
    filters: &[Filter],
    viewer: &str,
    community: &str,
    verifier: &Verifier,
    priority: Priority,
) -> Result<Vec<Verified>, String> {
    let body = serde_json::to_string(filters).expect("filters serialize");
    let bytes = http
        .post_with("/query", body, priority)
        .await
        .map_err(|error| error.to_string())?;
    // The envelope is bounded before parsing; the events keep their own
    // 8 MiB budget.
    if bytes.len() > crate::read_state::READ_SNAPSHOT_BODY_BYTES {
        return Err("Read-state snapshot response exceeds capacity".into());
    }
    let raw: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    parse_snapshot_response(&raw, viewer, community, verifier).map_err(|error| error.to_string())
}

/// Classifies a `POST /events` answer for a read-state publication.
fn publish_outcome(
    result: Result<Result<Vec<u8>, HttpError>, tokio::time::error::Elapsed>,
) -> Result<(), PublishFailure> {
    let body = match result {
        Err(_) => return Err(PublishFailure::Other("Read-state publish timed out".into())),
        Ok(Err(HttpError::Status {
            status,
            message,
            not_sent,
            ..
        })) => {
            let body = if not_sent {
                serde_json::json!({ "error": message, "sent": false })
            } else {
                serde_json::json!({ "error": message })
            };
            return Err(PublishFailure::from_response(status, &body));
        }
        Ok(Err(error)) => return Err(PublishFailure::Other(error.to_string())),
        Ok(Ok(body)) => body,
    };
    let receipt: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let message = receipt
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match receipt.get("accepted").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        Some(false) if message.starts_with("duplicate:") => Ok(()),
        Some(false) if message == TIMESTAMP_REFUSAL => Err(PublishFailure::TimestampRejected),
        Some(false) if !message.is_empty() => Err(PublishFailure::Other(message.to_owned())),
        Some(false) => Err(PublishFailure::Other(
            "Relay rejected the read state".into(),
        )),
        None => Err(PublishFailure::Other(
            "Relay returned an invalid receipt".into(),
        )),
    }
}

impl Actor {
    fn spawn_read(
        &self,
        filters: Vec<Filter>,
        options: ReadOptions,
        done: impl FnOnce(Result<Events, ReadError>) -> Message + Send + 'static,
    ) {
        let reader = self.reader.clone();
        let commands = self.commands.clone();
        self.runtime.spawn(async move {
            let result = tokio::time::timeout(READ_TIMEOUT, reader.read(filters, options))
                .await
                .unwrap_or_else(|_| {
                    Err(ReadError {
                        kind: crate::reader::ReadErrorKind::Unavailable,
                        message: "Relay read timed out".into(),
                        status: None,
                        retry_after: None,
                    })
                });
            let _ = commands.send(done(result));
        });
    }

    // ----- lifecycle -----

    /// After NIP-11: opens this partition's read-state journal and the
    /// unread owner, then refreshes markers (`reads.ensure()`).
    pub(super) fn open_reads(&mut self) {
        let Some(relay) = &self.relay else {
            return;
        };
        let viewer = self.identity.pubkey_hex();
        let partition = Partition::new(&self.origin, &viewer);
        let sync = ReadStateSync::open(
            viewer.clone(),
            ReadCapability::FrontierSync,
            SqliteReadJournal::new(self.store.clone(), partition.clone()),
        );
        let unread = Unread::with_follows(
            UnreadOptions {
                viewer,
                relay_author: Some(relay.authority.clone()),
                authority: Some(relay.authority.clone()),
            },
            SqliteThreadFollows::new(self.store.clone(), partition),
        );
        self.reads = Some(Reads {
            sync,
            unread,
            descriptor: relay.read_state_snapshot.clone(),
            channels: Vec::new(),
            refreshed: false,
            roster_ready: false,
            reconnecting: false,
            repair_wanted: true,
            repair_priority: Priority::Foreground,
            repair: None,
            sampled: false,
            lookup: None,
            lookup_after: None,
            reading: None,
            dwell: None,
            thread_reading: None,
            thread_dwell: None,
            seen: None,
            counts: Arc::new(UnreadCounts::default()),
        });
        self.admins_to_unread();
        self.refresh_reads(Priority::Foreground);
        self.update_reading();
    }

    /// Live dropped: ends the marker observation and fences queued intent
    /// (§3.5, first transition away from connected).
    pub(super) fn reads_disconnected(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        if reads.reconnecting {
            return;
        }
        reads.reconnecting = true;
        reads.sync.stale();
        reads.unread.stale();
        reads.sampled = false;
        if let Some(dwell) = reads.dwell.take() {
            reads.unread.end_reading(dwell.lease);
        }
        if let Some(dwell) = reads.thread_dwell.take() {
            reads.unread.end_reading(dwell.lease);
        }
        self.publish();
    }

    /// Live is back (global `established`): refresh markers, then repair
    /// evidence in the background (`unread.reconnect()`).
    pub(super) fn reads_reconnected(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        if !std::mem::take(&mut reads.reconnecting) {
            return;
        }
        reads.refreshed = false;
        if reads.unread.reconnect() {
            reads.repair_wanted = true;
            reads.repair_priority = Priority::Background;
        }
        self.refresh_reads(Priority::Background);
        self.schedule_dwell(Vec::new());
        self.schedule_thread_dwell();
    }

    /// The live minute tick (`recover` → `unread.reconnect()`): once evidence
    /// was asked for, refresh markers and repair it in the background.
    pub(super) fn reads_recover(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        if reads.reconnecting || !reads.unread.reconnect() {
            return;
        }
        reads.refreshed = false;
        reads.repair_wanted = true;
        reads.repair_priority = Priority::Background;
        self.refresh_reads(Priority::Background);
    }

    /// "Refresh" (`unread.refresh`): markers and then the evidence sample,
    /// both in the foreground.
    pub(super) fn refresh_unread(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        reads.refreshed = false;
        reads.repair_wanted = true;
        reads.repair_priority = Priority::Foreground;
        self.refresh_reads(Priority::Foreground);
    }

    /// "Retry read sync" (`unread.retrySync`): re-read markers, then publish
    /// what is waiting.
    pub(super) fn retry_read_sync(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        if let Some(command) = reads.sync.flush() {
            self.run_publish(command);
        }
        self.refresh_reads(Priority::Background);
    }

    /// The user asked to retry: rerun a failed refresh, repair or publish.
    pub(super) fn retry_reads(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        if reads.repair.is_none() && (reads.unread.error().is_some() || !reads.sampled) {
            reads.repair_wanted = true;
        }
        let failed = matches!(
            reads.sync.snapshot().status,
            ReadSyncStatus::Error | ReadSyncStatus::Stale
        );
        if let Some(command) = reads.sync.flush() {
            self.run_publish(command);
        }
        if failed {
            self.refresh_reads(Priority::Background);
        }
        self.pump_repair();
    }

    /// Hands the joined channels to unread; called whenever discovery may
    /// have changed access.
    pub(super) fn reads_channels(&mut self, roster_ready: bool) {
        let (Some(reads), Some(discovery)) = (&mut self.reads, &self.discovery) else {
            return;
        };
        let channels: Vec<ChannelInfo> = discovery
            .channels()
            .into_iter()
            .map(|channel| ChannelInfo {
                channel_type: channel.channel_type.map(|t| type_name(t).to_owned()),
                member: true,
                cached: channel.cached,
                archived: channel.archived,
                id: channel.id,
            })
            .collect();
        reads.channels = channels
            .iter()
            .filter(|channel| !channel.cached)
            .map(|channel| {
                (
                    channel.id.clone(),
                    channel.channel_type.as_deref() == Some("dm"),
                )
            })
            .collect();
        if reads.unread.set_channels(channels) {
            reads.repair_wanted = true;
        }
        reads.roster_ready |= roster_ready;
        self.pump_repair();
        self.schedule_dwell(Vec::new());
    }

    /// Feeds a verified batch to read state, then unread (the reference
    /// order). Returns whether the unread projection may have changed.
    pub(super) fn reads_accept(&mut self, events: &[Verified]) -> bool {
        let Some(reads) = &mut self.reads else {
            return false;
        };
        let before = (reads.unread.stamp(), reads.sync.changes());
        // Coverage builds the evidence index: only when read state needs it.
        if reads.sync.relevant(events) {
            let coverage = reads.unread.coverage();
            reads.sync.accept(events, &self.identity, Some(&coverage));
        }
        reads.unread.accept(events);
        before != (reads.unread.stamp(), reads.sync.changes())
    }

    /// New top-level messages in the channel being read, while its view is
    /// at the bottom, are in view: read them after the dwell.
    pub(super) fn reading_arrivals(&mut self, events: &[Verified]) {
        let Some(reading) = self.reads.as_ref().and_then(|reads| reads.reading.as_ref()) else {
            return;
        };
        if !reading.at_bottom {
            return;
        }
        let ids: Vec<String> = events
            .iter()
            .filter(|event| {
                event.channel() == Some(reading.channel.as_str())
                    && CONTENT_KINDS.contains(&event.kind_u16())
                    && (thread_reference(event).is_none() || event.has_tag("broadcast", "1"))
            })
            .map(Verified::id_hex)
            .collect();
        if !ids.is_empty() {
            self.schedule_dwell(ids);
        }
    }

    /// The channel's head or a page loaded: the bottom may be known now.
    pub(super) fn reading_loaded(&mut self, channel: &str) {
        if self
            .reads
            .as_ref()
            .and_then(|reads| reads.reading.as_ref())
            .is_some_and(|reading| reading.channel == channel)
        {
            self.schedule_dwell(Vec::new());
        }
    }

    // ----- marker refresh -----

    fn refresh_reads(&mut self, priority: Priority) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        // `None`: one is already running (this host always syncs).
        let Some(request) = reads.sync.begin_refresh(reads.descriptor.as_ref()) else {
            return;
        };
        let community = reads
            .descriptor
            .as_ref()
            .map(|descriptor| descriptor.community_id.clone())
            .unwrap_or_default();
        let (http, reader, verifier) = (
            self.http.clone(),
            self.reader.clone(),
            self.verifier.clone(),
        );
        let viewer = self.identity.pubkey_hex();
        let commands = self.commands.clone();
        self.runtime.spawn(async move {
            let read = async {
                if request.complete {
                    read_snapshot(
                        &http,
                        &request.filters,
                        &viewer,
                        &community,
                        &verifier,
                        priority,
                    )
                    .await
                } else {
                    reader
                        .read(
                            request.filters.clone(),
                            ReadOptions {
                                priority,
                                fresh: false,
                            },
                        )
                        .await
                        .map(|events| events.to_vec())
                        .map_err(|error| error.message)
                }
            };
            let result = tokio::time::timeout(READ_TIMEOUT, read)
                .await
                .unwrap_or_else(|_| Err("Read-state refresh timed out".into()));
            let _ = commands.send(Message::ReadRefresh { request, result });
        });
    }

    pub(super) fn read_refreshed(
        &mut self,
        request: RefreshRequest,
        result: Result<Vec<Verified>, String>,
    ) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        let coverage = reads.unread.coverage();
        reads
            .sync
            .finish_refresh(request, result, &self.identity, Some(&coverage), now_ms());
        // A result from before a disconnect was dropped; the reconnect
        // refreshes again.
        reads.refreshed |= !reads.reconnecting;
        self.pump_repair();
        self.publish();
    }

    // ----- publication -----

    /// Runs the network step the read-state owner asked for.
    fn run_publish(&self, command: PublishCommand) {
        let commands = self.commands.clone();
        match command {
            PublishCommand::Publish(event) => {
                let http = self.http.clone();
                let body = serde_json::to_string(&*event).expect("events serialize");
                // Only the exact own, encrypted coordinate leaves, at most
                // 64 KiB (`relay_publish_read_state`).
                if body.len() > crate::read_state::READ_STATE_EVENT_BYTES
                    || crate::read_state::decode_read_state(&self.identity, &event).is_err()
                {
                    let _ = commands.send(Message::ReadPublished(Err(PublishFailure::Other(
                        "Invalid read-state event".into(),
                    ))));
                    return;
                }
                self.runtime.spawn(async move {
                    let result =
                        tokio::time::timeout(READ_TIMEOUT, http.post("/events", body)).await;
                    let _ = commands.send(Message::ReadPublished(publish_outcome(result)));
                });
            }
            PublishCommand::ReadBack(filters) => {
                let options = ReadOptions {
                    fresh: true,
                    ..ReadOptions::default()
                };
                self.spawn_read(filters, options, |result| {
                    Message::ReadBack(
                        result
                            .map(|events| events.to_vec())
                            .map_err(|error| error.message),
                    )
                });
            }
        }
    }

    pub(super) fn read_published(&mut self, outcome: Result<(), PublishFailure>) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        if let Some(command) = reads.sync.published(outcome, now_ms()) {
            self.run_publish(command);
        }
        self.publish();
    }

    pub(super) fn read_back(&mut self, result: Result<Vec<Verified>, String>) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        let coverage = reads.unread.coverage();
        let next = reads
            .sync
            .read_back(result, &self.identity, Some(&coverage), now_ms());
        if let Some(command) = next {
            self.run_publish(command);
        }
        self.publish();
    }

    // ----- timers -----

    /// The next wall-clock time [`Self::reads_tick`] has work.
    pub(super) fn reads_deadline(&self) -> Option<u64> {
        let reads = self.reads.as_ref()?;
        let lookup = reads
            .lookup_after
            .filter(|_| reads.lookup.is_none() && reads.unread.lookups_pending());
        // A running publication defers the debounce; it is not due yet.
        let publish = reads.sync.deadline().filter(|_| !reads.sync.publishing());
        [
            publish,
            reads.dwell.as_ref().map(|dwell| dwell.due),
            reads.thread_dwell.as_ref().map(|dwell| dwell.due),
            lookup,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(super) fn reads_tick(&mut self) {
        let now = now_ms();
        let Some(reads) = &mut self.reads else {
            return;
        };
        if let Some(command) = reads.sync.poll(now) {
            self.run_publish(command);
        }
        if self
            .reads
            .as_ref()
            .and_then(|reads| reads.dwell.as_ref())
            .is_some_and(|dwell| dwell.due <= now)
        {
            self.fire_dwell();
        }
        if self
            .reads
            .as_ref()
            .and_then(|reads| reads.thread_dwell.as_ref())
            .is_some_and(|dwell| dwell.due <= now)
        {
            self.fire_thread_dwell();
        }
        if let Some(reads) = &mut self.reads
            && reads.lookup_after.is_some_and(|after| after <= now)
        {
            reads.lookup_after = None;
            self.pump_lookups();
        }
    }

    // ----- evidence repair -----

    /// Starts the sampled evidence read once markers are loaded and the
    /// roster is known, when one is wanted and none runs.
    fn pump_repair(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        if !reads.repair_wanted || !reads.refreshed || !reads.roster_ready || reads.repair.is_some()
        {
            return;
        }
        let Some(repair) = reads.unread.begin_repair() else {
            return;
        };
        reads.repair_wanted = false;
        let priority = std::mem::replace(&mut reads.repair_priority, Priority::Foreground);
        reads.repair = Some(RepairRun {
            repair,
            next: 0,
            priority,
        });
        self.repair_next();
    }

    fn repair_next(&mut self) {
        let Some(run) = self.reads.as_mut().and_then(|reads| reads.repair.as_mut()) else {
            return;
        };
        let Some(filter) = run.repair.filters.get(run.next).cloned() else {
            self.finish_repair(Ok(()));
            return;
        };
        run.next += 1;
        let options = ReadOptions {
            priority: run.priority,
            fresh: false,
        };
        self.spawn_read(vec![filter], options, Message::Repair);
    }

    pub(super) fn repaired(&mut self, result: Result<Events, ReadError>) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        let Some(run) = &mut reads.repair else {
            return;
        };
        match result {
            Ok(events) => {
                let more = reads.unread.repair_batch(&mut run.repair, &events);
                self.publish();
                if more {
                    self.repair_next();
                } else {
                    self.finish_repair(Ok(()));
                }
            }
            Err(error) => self.finish_repair(Err(error.message)),
        }
    }

    fn finish_repair(&mut self, result: Result<(), String>) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        let Some(run) = reads.repair.take() else {
            return;
        };
        let end = reads.unread.finish_repair(run.repair, result);
        reads.repair_wanted |= end.again;
        if end.find_homes {
            reads.sampled = true;
            if let Some(request) = reads.unread.homes_request(&reads.sync) {
                let filters = request.filters.clone();
                self.spawn_read(filters, ReadOptions::background(), move |result| {
                    Message::Homes { request, result }
                });
            }
        }
        self.pump_repair();
        self.publish();
    }

    pub(super) fn homes(&mut self, request: HomesRequest, result: Result<Events, ReadError>) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        let Ok(events) = result else {
            return;
        };
        if let Some(found) = reads.unread.homes_found(request, &events) {
            let coverage = reads.unread.coverage();
            if let Err(error) = reads.sync.learn_homes(&found, Some(&coverage)) {
                tracing::warn!(%error, "could not save read-state homes");
            }
            self.publish();
        }
    }

    // ----- conversation lookups (§5i.8) -----

    fn pump_lookups(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        if reads.lookup.is_some()
            || reads.lookup_after.is_some_and(|after| after > now_ms())
            || !reads.unread.lookups_pending()
        {
            return;
        }
        let Some(batch) = reads.unread.next_lookup_batch() else {
            return;
        };
        let mut pages = VecDeque::new();
        if !batch.reply_targets.is_empty() {
            pages.push_back(RepliesPage {
                targets: batch.reply_targets.clone(),
                until: None,
                pages: 0,
            });
        }
        reads.lookup = Some(LookupRun {
            batch,
            fetched: Vec::new(),
            replies: Vec::new(),
            structure_read: false,
            pages,
            current: None,
        });
        self.lookup_step();
    }

    fn lookup_step(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        let Some(run) = &mut reads.lookup else {
            return;
        };
        let structure = if run.structure_read {
            None
        } else {
            run.structure_read = true;
            run.batch.ids_filter.clone()
        };
        let filter = structure.or_else(|| {
            let page = run.pages.pop_front()?;
            let filter =
                reads
                    .unread
                    .replies_filter(&run.batch.channel_id, &page.targets, page.until);
            run.current = Some(page);
            Some(filter)
        });
        match filter {
            Some(filter) => {
                self.spawn_read(vec![filter], ReadOptions::background(), Message::Lookup);
            }
            None => {
                let run = reads.lookup.take().expect("running");
                reads
                    .unread
                    .complete_lookup(run.batch, &run.fetched, &run.replies);
                self.publish();
            }
        }
    }

    pub(super) fn looked_up(&mut self, result: Result<Events, ReadError>) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        let Some(run) = &mut reads.lookup else {
            return;
        };
        let events = match result {
            Ok(events) => events,
            Err(_) => {
                let run = reads.lookup.take().expect("running");
                let delay = reads.unread.fail_lookup(run.batch).unwrap_or(0);
                reads.lookup_after = Some(now_ms() + delay);
                return;
            }
        };
        match run.current.take() {
            None => run.fetched.extend(events.iter().cloned()),
            Some(page) => {
                match replies_paging(&page.targets, &events, page.until, page.pages) {
                    RepliesPaging::Done => {}
                    RepliesPaging::Split(at) => {
                        let (first, rest) = page.targets.split_at(at);
                        for targets in [first, rest] {
                            run.pages.push_back(RepliesPage {
                                targets: targets.to_vec(),
                                until: page.until,
                                pages: page.pages,
                            });
                        }
                    }
                    RepliesPaging::Older { until } => run.pages.push_back(RepliesPage {
                        targets: page.targets,
                        until: Some(until),
                        pages: page.pages + 1,
                    }),
                }
                run.replies.extend(events.iter().cloned());
            }
        }
        self.lookup_step();
    }

    // ----- explicit marks -----

    /// Marks, answering the channels a mark of everything read.
    pub(super) fn mark(&mut self, mark: Mark) -> Result<Vec<(String, ReadMutationResult)>, String> {
        let reads = self
            .reads
            .as_mut()
            .ok_or("Not connected to the relay yet")?;
        let now = now_ms();
        let (sync, unread) = (&mut reads.sync, &mut reads.unread);
        let mut marked = Vec::new();
        let result = match mark {
            Mark::Channel(channel) => unread
                .mark_channel_read(sync, &channel, now / 1000, now)
                .map(drop),
            Mark::All => unread
                .mark_all_channels_read(sync, now / 1000, now)
                .map(|results| marked = results),
            Mark::MessageRead(channel, id) => unread
                .mark_message_read(sync, &channel, &id.to_hex(), now)
                .map(drop),
            Mark::MessageUnread(channel, id) => unread
                .mark_message_unread(sync, &channel, &id.to_hex(), now)
                .map(drop),
            Mark::ClearChannelUnread(channel) => unread
                .clear_unread_local(sync, &ReadTarget::channel(channel), now)
                .map(drop),
            Mark::ChannelUnread(channel) => unread
                .mark_unread_local(sync, &ReadTarget::channel(channel), now)
                .map(drop),
            Mark::Local {
                target,
                clear: true,
            } => unread.clear_unread_local(sync, &target, now).map(drop),
            Mark::Local {
                target,
                clear: false,
            } => unread.mark_unread_local(sync, &target, now).map(drop),
            Mark::Through {
                channel,
                root,
                message,
            } => {
                let target = match root {
                    Some(root) => ReadTarget::thread(channel, root.to_hex()),
                    None => ReadTarget::channel(channel),
                };
                unread
                    .mark_through(sync, &target, &message.to_hex(), now)
                    .map(drop)
            }
        };
        self.publish();
        result.map(|()| marked).map_err(|error| error.to_string())
    }

    // ----- reading -----

    /// The selected channel with an open view is the one being read.
    pub(super) fn update_reading(&mut self) {
        let next = self
            .selected
            .clone()
            .filter(|id| self.timelines.get(id).is_some_and(|t| t.viewers > 0));
        let Some(reads) = &mut self.reads else {
            return;
        };
        if reads.reading.as_ref().map(|reading| &reading.channel) == next.as_ref() {
            return;
        }
        if let Some(dwell) = reads.dwell.take() {
            reads.unread.end_reading(dwell.lease);
        }
        if let Some(previous) = reads.reading.take() {
            reads.unread.leave_channel(&previous.channel);
        }
        let Some(channel) = next else {
            self.publish();
            return;
        };
        // Fails until the roster lists the channel; retried at the dwell.
        let _ = reads
            .unread
            .enter_channel(&mut reads.sync, &channel, now_ms());
        // Nothing is in view until the view says so: a hidden or unfocused
        // window reads nothing.
        let (visible, at_bottom) = self
            .view_reports
            .get(&channel)
            .map(|(visible, at_bottom)| (visible.iter().map(EventId::to_hex).collect(), *at_bottom))
            .unwrap_or_default();
        reads.reading = Some(Reading {
            channel,
            at_bottom,
            visible,
        });
        self.schedule_dwell(Vec::new());
        self.publish();
    }

    /// What the view of `channel` shows now. Kept while the view is open,
    /// for when the channel is selected again.
    pub(super) fn viewport(&mut self, channel: &str, visible: Vec<EventId>, at_bottom: bool) {
        let start = visible.len().saturating_sub(VIEW_IDS);
        let visible = visible[start..].to_vec();
        self.view_reports
            .insert(channel.to_owned(), (visible.clone(), at_bottom));
        let Some(reading) = self
            .reads
            .as_mut()
            .and_then(|reads| reads.reading.as_mut())
            .filter(|reading| reading.channel == channel)
        else {
            return;
        };
        reading.visible = visible.iter().map(EventId::to_hex).collect();
        reading.at_bottom = at_bottom;
        self.schedule_dwell(Vec::new());
    }

    /// What thread panel `view` shows now: its fully visible replies, and
    /// whether it sits at its newest reply.
    pub(super) fn thread_viewport(&mut self, view: u64, visible: Vec<EventId>, at_bottom: bool) {
        let Some(thread) = self.threads.get(&view) else {
            return;
        };
        let channel = thread.channel.clone();
        let snapshot = thread.snapshot();
        // A missing root must not mean catching up the thread
        // (`ThreadPanel.tsx`): only a loaded root has a bottom.
        let bottom = at_bottom
            .then(|| {
                let root = snapshot.root.as_ref()?;
                let newest = snapshot
                    .replies
                    .iter()
                    .max_by(|a, b| a.created_at.cmp(&b.created_at).then(b.id.cmp(&a.id)))?;
                Some((newest.id.to_hex(), root.id.to_hex()))
            })
            .flatten();
        let Some(reads) = &mut self.reads else {
            return;
        };
        let start = visible.len().saturating_sub(VIEW_IDS);
        let visible: Vec<String> = visible[start..].iter().map(EventId::to_hex).collect();
        if visible.is_empty() && bottom.is_none() {
            if reads
                .thread_reading
                .as_ref()
                .is_some_and(|reading| reading.view == view)
            {
                reads.thread_reading = None;
                if let Some(dwell) = reads.thread_dwell.take() {
                    reads.unread.end_reading(dwell.lease);
                }
            }
            return;
        }
        reads.thread_reading = Some(ThreadReading {
            view,
            channel,
            visible,
            bottom,
        });
        self.schedule_thread_dwell();
    }

    /// Thread panel `view` closed: it reads nothing more.
    pub(super) fn thread_closed(&mut self, view: u64) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        if reads
            .thread_reading
            .as_ref()
            .is_some_and(|reading| reading.view == view)
        {
            reads.thread_reading = None;
            if let Some(dwell) = reads.thread_dwell.take() {
                reads.unread.end_reading(dwell.lease);
            }
        }
    }

    fn schedule_thread_dwell(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        if let Some(dwell) = reads.thread_dwell.take() {
            reads.unread.end_reading(dwell.lease);
        }
        let Some(reading) = &reads.thread_reading else {
            return;
        };
        let Ok(lease) = reads.unread.reading(&reading.channel, &reads.sync) else {
            return;
        };
        reads.unread.view(&lease, &reading.visible, &reads.sync);
        reads.thread_dwell = Some(ThreadDwell {
            lease,
            ids: reading.visible.clone(),
            bottom: reading.bottom.clone(),
            due: now_ms() + READING_DWELL_MS,
        });
    }

    /// The thread dwell elapsed: catch up the thread through its newest
    /// reply (`catchUp(bottom, rootId)`) and observe what stayed in view.
    fn fire_thread_dwell(&mut self) {
        let now = now_ms();
        let Some(reads) = &mut self.reads else {
            return;
        };
        let Some(mut dwell) = reads.thread_dwell.take() else {
            return;
        };
        let (sync, unread) = (&mut reads.sync, &mut reads.unread);
        if let Some((newest, root)) = &dwell.bottom
            && let Err(error) = unread.catch_up(sync, &mut dwell.lease, newest, Some(root), now)
        {
            tracing::debug!(%error, root, "thread catch-up skipped");
        }
        for id in &dwell.ids {
            let _ = unread.observe(sync, &mut dwell.lease, std::slice::from_ref(id), now);
        }
        unread.end_reading(dwell.lease);
        self.publish();
    }

    /// Restarts the dwell over what is in view plus `arrived`, under a new
    /// lease. Nothing in view (and not at the bottom) ends it.
    fn schedule_dwell(&mut self, arrived: Vec<String>) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        let Some(reading) = &reads.reading else {
            return;
        };
        if let Some(dwell) = reads.dwell.take() {
            reads.unread.end_reading(dwell.lease);
        }
        let mut ids = reading.visible.clone();
        let known: HashSet<String> = ids.iter().cloned().collect();
        ids.extend(arrived.into_iter().filter(|id| !known.contains(id)));
        let start = ids.len().saturating_sub(VIEW_IDS);
        let ids = ids.split_off(start);
        if ids.is_empty() && !reading.at_bottom {
            return;
        }
        let Ok(lease) = reads.unread.reading(&reading.channel, &reads.sync) else {
            return;
        };
        reads.unread.view(&lease, &ids, &reads.sync);
        reads.dwell = Some(Dwell {
            lease,
            ids,
            bottom: reading.at_bottom,
            due: now_ms() + READING_DWELL_MS,
        });
    }

    /// The dwell elapsed: catch up the bottom and observe what stayed in
    /// view, then end the lease.
    fn fire_dwell(&mut self) {
        let now = now_ms();
        let Some(reads) = &mut self.reads else {
            return;
        };
        let Some(mut dwell) = reads.dwell.take() else {
            return;
        };
        let channel = dwell.lease.channel_id().to_owned();
        let (sync, unread) = (&mut reads.sync, &mut reads.unread);
        let _ = unread.enter_channel(sync, &channel, now);
        if dwell.bottom
            && let Some(bottom) = self.timelines.get(&channel).and_then(|t| t.bottom())
            && let Err(error) = unread.catch_up(sync, &mut dwell.lease, &bottom, None, now)
        {
            tracing::debug!(%error, channel, "reading catch-up skipped");
        }
        for id in &dwell.ids {
            // One unavailable row must not stop the others.
            let _ = unread.observe(sync, &mut dwell.lease, std::slice::from_ref(id), now);
        }
        unread.end_reading(dwell.lease);
        self.publish();
    }

    // ----- projection -----

    /// The sidebar projection, recomputed only when unread or read state
    /// changed and replaced only when it differs.
    pub(super) fn unread_counts(&mut self) -> Arc<UnreadCounts> {
        let Some(reads) = &mut self.reads else {
            return self.snapshot.borrow().unread.clone();
        };
        let key = (reads.unread.stamp(), reads.sync.changes(), reads.sampled);
        if reads.seen == Some(key) {
            return reads.counts.clone();
        }
        let mut counts = UnreadCounts {
            observed: reads.sampled,
            sync: Some(reads.sync.snapshot().clone()),
            ..UnreadCounts::default()
        };
        for (id, dm) in &reads.channels {
            let snapshot = reads
                .unread
                .snapshot(&ReadTarget::channel(id.as_str()), &reads.sync);
            if let Some(latest) = &snapshot.latest_message {
                counts.latest.insert(id.clone(), latest.created_at);
                counts.previews.insert(id.clone(), latest.preview.clone());
            }
            let activity = reads.unread.activity(id, &reads.sync);
            let threads = activity.items.as_ref().map_or(0, Vec::len) as u32;
            // Every channel whose read state is known, none unread
            // included, as `threads/activity` answers (`computeActivity`).
            if activity.items.is_some() {
                counts.activity.insert(id.clone(), activity.clone());
            }
            if !snapshot.unread() && threads == 0 {
                continue;
            }
            let badge = snapshot.badge(*dm);
            let row = ChannelUnread {
                unread: badge.unread,
                bold: badge.bold,
                priority: badge.priority,
                count: snapshot.observed_count.unwrap_or(0),
                attention: snapshot.attention_count.unwrap_or(0),
                mentions: snapshot.mention_count.unwrap_or(0),
                manual: snapshot.manual,
                stale: snapshot.freshness == Freshness::Stale,
                threads,
                threads_stale: activity.freshness == Freshness::Stale,
            };
            if row.unread {
                counts.mentions += row.mentions;
                if *dm {
                    counts.direct += row.count;
                }
            }
            counts.channels.insert(id.clone(), row);
        }
        // Computing snapshots may have queued conversation lookups.
        reads.seen = Some((reads.unread.stamp(), reads.sync.changes(), reads.sampled));
        if *reads.counts != counts {
            reads.counts = Arc::new(counts);
        }
        reads.counts.clone()
    }

    /// Reports each [`UnreadWatch`] whose rows' unread state may have
    /// changed since its last report.
    pub(super) fn publish_watches(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        for watch in self.watches.values_mut() {
            let key = (reads.unread.stamp(), reads.sync.changes());
            if watch.seen == Some(key) {
                continue;
            }
            let channel = watch.channel.as_str();
            let mut detail = UnreadDetail::default();
            for root in &watch.threads {
                let snapshot = reads
                    .unread
                    .snapshot(&ReadTarget::thread(channel, root.to_hex()), &reads.sync);
                detail.threads.insert(
                    *root,
                    ThreadUnread {
                        manual: snapshot.manual,
                        observed: snapshot.observed_count,
                        stale: snapshot.freshness == Freshness::Stale,
                    },
                );
            }
            for id in &watch.messages {
                let attention = reads.unread.attention(channel, &id.to_hex(), &reads.sync);
                if attention.unread {
                    detail.unread.insert(*id);
                }
                if attention.forced {
                    detail.forced.insert(*id);
                }
            }
            // Computing may have queued lookups, which change the stamp.
            watch.seen = Some((reads.unread.stamp(), reads.sync.changes()));
            watch.sender.send_if_modified(|current| {
                let changed = **current != detail;
                if changed {
                    *current = Arc::new(detail);
                }
                changed
            });
        }
    }

    /// Re-checks held notification candidates, then starts queued
    /// conversation lookups (including any the re-check queued); after
    /// each publish.
    pub(super) fn after_publish(&mut self) {
        self.pump_incoming();
        self.pump_lookups();
    }
}
