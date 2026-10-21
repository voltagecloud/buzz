//! Channel heads, tails, catch-up and the device cache inside the session
//! actor (`store.ts`, `persistence.ts`, the catch-up part of `session.ts`).
//!
//! - A *head* is the newest page of one channel, kept for 64 channels
//!   (4 MiB) so returning to a channel or hovering it does not wait for the
//!   relay. Heads read from the relay are saved to disk with their authors'
//!   profiles; saved heads are re-verified and shown, marked cached, until
//!   the relay confirms them.
//! - A *tail* is the newest 256 live events of a channel without a window,
//!   for its sidebar preview and to seed the window when it opens.
//! - After the live stream is (re)established, every retained window
//!   catches up with one head read, at most two at a time, the selected
//!   channel first.
//! - The roster is saved too, so a later start shows the saved sidebar and
//!   conversations at once, also while the relay cannot be reached.

use super::timeline::{FRESH_FOR_MS, MAX_BYTES, MAX_ROWS, Timeline, message_preview, rows_people};
use super::unread::now_ms;
use super::{Actor, CatchUpState, HeadCatchUp, Message};
use crate::budget::ByteLru;
use crate::discovery::DiscoveryState;
use crate::event::{Verified, Verifier, is_hex64, kinds, newest_first};
use crate::fold::{FoldOptions, fold_messages};
use crate::reader::{Events, Priority, ReadError, ReadErrorKind, ReadOptions, Reader};
use crate::socket::Filter;
use crate::store::{MAX_SAVED_AGE_MS, MAX_SAVED_BYTES, Partition, Store};
use crate::window::{Cursor, PAGE_SIZE, page_filter, parse_page};
use nostr::prelude::{EventId, PublicKey};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::time::Instant;

/// Heads kept in memory (`store.ts` `heads`).
const MAX_HEADS: usize = 64;
const MAX_HEAD_BYTES: usize = 4 << 20;
/// Channels with a tail, and their total size.
const MAX_TAILS: usize = 64;
const MAX_TAIL_BYTES: usize = 4 << 20;
/// Newest live events per tail.
const TAIL_EVENTS: usize = 256;
/// Retained catch-ups at once; only the selected channel uses a second.
const CATCH_UP_SLOTS: usize = 2;
/// Saved roster: events and the participants' profiles.
const MAX_SAVED_ROSTER: usize = 2048;
const MAX_SAVED_PROFILES: usize = 1024;
/// Session channels page flat, with their overlays read separately.
const SESSION_OVERLAY_EVENTS: usize = 2000;
const SESSION_OVERLAY_BYTES: usize = 4 << 20;
/// Why a cached-only session reads nothing.
pub(super) const RECONNECT_TO_UPDATE: &str = "Reconnect to update saved conversations.";

/// One channel's newest page.
#[derive(Clone)]
pub(super) struct Head {
    pub events: Arc<[Verified]>,
    pub cursor: Option<Cursor>,
    pub has_more: bool,
    /// When it was read from the relay, in Unix ms.
    pub saved_at: u64,
    /// Not confirmed since a disconnect, or restored from disk.
    pub cached: bool,
    pub restored: bool,
    pub preview: Option<String>,
    /// Everyone its rows show, for profiles and the saved copy.
    pub people: Vec<String>,
}

impl Head {
    fn bytes(&self) -> usize {
        self.events.iter().map(Verified::byte_size).sum::<usize>() + self.events.len() + 2
    }

    fn fresh(&self, now: u64) -> bool {
        !self.cached && now.saturating_sub(self.saved_at) < FRESH_FOR_MS
    }
}

/// A channel's newest live events while it has no window.
#[derive(Default)]
struct Tail {
    /// Newest first.
    events: Vec<Verified>,
    preview: Option<String>,
}

impl Tail {
    /// Puts `event` in order, keeping the newest [`TAIL_EVENTS`]. Returns
    /// whether the tail changed.
    fn insert(&mut self, event: &Verified) -> bool {
        let at = self.events.binary_search_by(|probe| {
            event
                .created_at
                .cmp(&probe.created_at)
                .then_with(|| probe.id.cmp(&event.id))
        });
        match at {
            Err(at) if at < TAIL_EVENTS => {
                self.events.insert(at, event.clone());
                self.events.truncate(TAIL_EVENTS);
                true
            }
            // Already kept, or older than everything kept.
            _ => false,
        }
    }

    fn bytes(&self) -> usize {
        self.events.iter().map(Verified::byte_size).sum::<usize>()
            + self.preview.as_ref().map_or(0, String::len)
            + 2
    }
}

/// What a page read was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PageFor {
    /// A window's head or an older page, fenced by its generation.
    Window(u64),
    /// Hover or intent: warm the head before the channel opens.
    Prepare,
    /// The post-establishment catch-up of this live generation.
    CatchUp(u64),
}

/// A page as read, before the session admits it.
pub(super) enum PageRead {
    /// A signed-bound window page; the bound is checked after admission.
    Window(Events),
    /// Flat session paging: rows with their overlays, and the next cursor.
    Session {
        events: Vec<Verified>,
        cursor: Option<Cursor>,
        has_more: bool,
    },
}

#[derive(Clone, Debug)]
pub(super) struct Catchup {
    generation: u64,
    state: CatchUpState,
    error: Option<String>,
    retry_at: Option<Instant>,
}

/// The device cache as restored at startup, already verified.
pub(super) struct Restored {
    author: String,
    roster: Vec<Verified>,
    profiles: Vec<Verified>,
    heads: Vec<RestoredHead>,
}

pub(super) struct RestoredHead {
    channel: String,
    saved_ms: u64,
    events: Vec<Verified>,
    profiles: Vec<Verified>,
}

pub(super) struct Channels {
    heads: ByteLru<Head>,
    tails: ByteLru<Tail>,
    /// Channel admins from relay-signed 39001s (see `admins.rs`).
    pub admins: ByteLru<super::admins::ChannelAdmins>,
    /// One speculative head read at a time.
    preparing: bool,
    catchups: HashMap<String, Catchup>,
    queue: VecDeque<String>,
    running: HashSet<String>,
    /// Bumped whenever the live stream drops; fences catch-ups.
    pub live_generation: u64,
    /// Bumped per channel when its access changes; fences its reads.
    access: HashMap<String, u64>,
    /// The channel to restore first: the one last open.
    pub initial: Option<String>,
    /// The relay identity the startup cache was saved under, while the
    /// session shows it before NIP-11 confirms that identity.
    pub cached_author: Option<String>,
    /// Network roster evidence arrived; the saved one no longer applies.
    pub discovery_observed: bool,
    /// Saved heads were restored once; an access revocation then wipes
    /// them.
    hydrated: bool,
    startup_writing: bool,
    startup_dirty: bool,
}

impl Default for Channels {
    fn default() -> Self {
        Self {
            heads: ByteLru::new(MAX_HEADS, MAX_HEAD_BYTES),
            tails: ByteLru::new(MAX_TAILS, MAX_TAIL_BYTES),
            admins: super::admins::cache(),
            preparing: false,
            catchups: HashMap::new(),
            queue: VecDeque::new(),
            running: HashSet::new(),
            live_generation: 0,
            access: HashMap::new(),
            initial: None,
            cached_author: None,
            discovery_observed: false,
            hydrated: false,
            startup_writing: false,
            startup_dirty: false,
        }
    }
}

fn session_invalid(message: &str) -> ReadError {
    ReadError {
        kind: ReadErrorKind::InvalidResponse,
        message: message.into(),
        status: None,
        retry_after: None,
    }
}

fn paging(filter: &mut Value, cursor: Option<&Cursor>) {
    if let Some(cursor) = cursor {
        filter["until"] = json!(cursor.created_at);
        filter["before_id"] = json!(cursor.id);
    }
}

/// Strictly older than the cursor.
fn after(event: &Verified, cursor: &Cursor) -> bool {
    event.created_at_secs() < cursor.created_at
        || (event.created_at_secs() == cursor.created_at && event.id_hex() > cursor.id)
}

fn cursor_of(event: &Verified) -> Cursor {
    Cursor {
        created_at: event.created_at_secs(),
        id: event.id_hex(),
    }
}

/// Every overlay of `ids` of these kinds, paged through to an empty page.
async fn session_overlays(
    reader: &Reader,
    options: ReadOptions,
    kinds: &[u16],
    ids: Vec<String>,
    events: &mut Vec<Verified>,
) -> Result<Vec<Verified>, ReadError> {
    let mut found = Vec::new();
    if ids.is_empty() {
        return Ok(found);
    }
    let mut next: Option<Cursor> = None;
    loop {
        let mut filter = json!({"kinds": kinds, "#e": ids, "limit": 500});
        paging(&mut filter, next.as_ref());
        let mut page: Vec<Verified> = reader
            .read(vec![crate::socket::filter(filter)], options)
            .await?
            .to_vec();
        page.sort_by(|a, b| newest_first(a, b));
        let Some(last) = page.last().cloned() else {
            return Ok(found);
        };
        if next
            .as_ref()
            .is_some_and(|cursor| page.iter().any(|event| !after(event, cursor)))
        {
            return Err(session_invalid(
                "Session message updates did not advance. Retry messages.",
            ));
        }
        found.extend(page.iter().cloned());
        events.extend(page);
        let bytes: usize = events.iter().map(Verified::byte_size).sum();
        if events.len() > SESSION_OVERLAY_EVENTS || bytes > SESSION_OVERLAY_BYTES {
            return Err(session_invalid(
                "Session message updates exceed the read budget.",
            ));
        }
        // Short pages continue: visibility can shrink a response.
        next = Some(cursor_of(&last));
    }
}

/// Reads one page of a session channel (`session-window.ts`): rows by
/// time, then their deletions, reactions and edits, then the deletions of
/// those. General queries have no signed bounds, so paging continues until
/// an empty page.
pub(super) async fn read_session_window(
    reader: &Reader,
    channel: &str,
    sent: Option<&Cursor>,
    options: ReadOptions,
) -> Result<PageRead, ReadError> {
    let mut filter = json!({
        "kinds": kinds::CHANNEL_ROW_KINDS,
        "#h": [channel],
        "limit": PAGE_SIZE,
    });
    paging(&mut filter, sent);
    let response = reader
        .read(vec![crate::socket::filter(filter)], options)
        .await?;
    let mut seen = HashSet::new();
    let mut rows: Vec<Verified> = response
        .iter()
        .filter(|event| {
            seen.insert(event.id)
                && kinds::CHANNEL_ROW_KINDS.contains(&event.kind_u16())
                && event.has_tag("h", channel)
        })
        .cloned()
        .collect();
    rows.sort_by(|a, b| newest_first(a, b));
    if sent.is_some_and(|cursor| rows.iter().any(|event| !after(event, cursor))) {
        return Err(session_invalid(
            "Session history did not advance. Retry messages.",
        ));
    }
    let mut events = rows.clone();
    let aux = session_overlays(
        reader,
        options,
        &[
            kinds::DELETION,
            kinds::REACTION,
            kinds::MODERATOR_DELETE,
            kinds::EDIT,
        ],
        rows.iter().map(Verified::id_hex).collect(),
        &mut events,
    )
    .await?;
    session_overlays(
        reader,
        options,
        &[kinds::DELETION, kinds::MODERATOR_DELETE],
        aux.iter()
            .filter(|event| matches!(event.kind_u16(), kinds::REACTION | kinds::EDIT))
            .map(Verified::id_hex)
            .collect(),
        &mut events,
    )
    .await?;
    let cursor = rows.last().map(cursor_of);
    Ok(PageRead::Session {
        events,
        has_more: cursor.is_some(),
        cursor,
    })
}

/// A channel's own head read (`isChannelHead`): what a catch-up replaces.
pub(super) fn is_channel_head(filters: &[Filter], channel: &str) -> bool {
    filters.iter().any(|filter| {
        filter
            .get("#h")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(channel)))
            && filter.get("top_level") == Some(&Value::Bool(true))
            && !filter.contains_key("until")
            && !filter.contains_key("depth_limit")
            && filter
                .get("kinds")
                .and_then(Value::as_array)
                .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_u64() == Some(9)))
    })
}

/// Reads and verifies the device cache for one partition, off the actor
/// (`restoreStartup`, `hydrate`). Anything malformed is left out: a corrupt
/// cache never blocks the network.
pub(super) fn restore(
    store: &Store,
    partition: &Partition,
    verifier: &Verifier,
) -> Option<Restored> {
    let now = now_ms();
    let raw = store.startup_discovery(partition).ok()??;
    if raw.len() > MAX_SAVED_BYTES {
        return None;
    }
    let saved: Value = serde_json::from_str(&raw).ok()?;
    let author = saved.get("relayAuthor")?.as_str()?.to_string();
    let saved_at = saved.get("savedAt")?.as_u64()?;
    let valid_author = is_hex64(&author);
    if !valid_author || saved_at > now || now - saved_at > MAX_SAVED_AGE_MS as u64 {
        return None;
    }
    let events = saved.get("events")?.as_array()?;
    if events.len() > MAX_SAVED_ROSTER {
        return None;
    }
    // One bad roster event discards the snapshot.
    let roster = verifier.verify_all(events).ok()?;
    let profiles = saved
        .get("profiles")
        .and_then(Value::as_array)
        .filter(|profiles| profiles.len() <= MAX_SAVED_PROFILES)
        .map(|profiles| {
            profiles
                .iter()
                .filter_map(|value| verifier.verify_json(value).ok())
                .collect()
        })
        .unwrap_or_default();
    let heads = store
        .load_heads(partition, now as i64)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|head| {
            // A corrupt or unsigned record cannot reach the read model.
            let events = verifier.verify_all(&head.events).ok()?;
            let profiles = verifier.verify_all(&head.profiles).ok()?;
            Some(RestoredHead {
                channel: head.channel,
                saved_ms: head.saved_ms as u64,
                events,
                profiles,
            })
        })
        .collect();
    Some(Restored {
        author,
        roster,
        profiles,
        heads,
    })
}

impl Actor {
    /// May this channel be read from the relay now (`canReadRemote`)? Not
    /// before NIP-11, and not while its membership is only saved.
    pub(super) fn can_read_remote(&self, channel: &str) -> bool {
        self.relay.is_some()
            && self.discovery.as_ref().is_some_and(|discovery| {
                discovery.can_access(channel) && !discovery.is_cached(channel)
            })
    }

    fn is_session(&self, channel: &str) -> bool {
        self.discovery
            .as_ref()
            .is_some_and(|discovery| discovery.is_session(channel))
    }

    pub(super) fn access_version(&self, channel: &str) -> u64 {
        self.channels.access.get(channel).copied().unwrap_or(0)
    }

    /// Bumps a channel's access version, fencing reads started before.
    pub(super) fn bump_access(&mut self, channel: &str) {
        *self.channels.access.entry(channel.to_string()).or_default() += 1;
    }

    fn relay_key(&self) -> Option<PublicKey> {
        let author = match &self.relay {
            Some(relay) => relay.authority.as_str(),
            None => self.channels.cached_author.as_deref()?,
        };
        PublicKey::from_hex(author).ok()
    }

    fn fold_options(&self, channel: &str, relay: PublicKey) -> FoldOptions {
        FoldOptions {
            signing_authority: Some(relay),
            include_replies: self.is_session(channel),
            channel_admins: self.channel_admins(channel),
        }
    }

    // ----- windows -----

    /// The window for `channel`, created (with the channel's tail as its
    /// traffic) when missing (`touch`).
    pub(super) fn touch(&mut self, channel: &str) -> Option<&mut Timeline> {
        if !self.timelines.contains_key(channel) {
            let relay = self.relay_key()?;
            self.next_generation += 1;
            let mut timeline = Timeline::new(
                channel.to_string(),
                relay,
                self.clock.clone(),
                self.next_generation,
                self.is_session(channel),
            );
            if let Some(tail) = self.channels.tails.peek(channel) {
                timeline.seed_traffic(&tail.events);
            }
            let local = self.local_operations();
            timeline.set_local(&local);
            timeline.set_admins(&self.channel_admins(channel));
            self.timelines.insert(channel.to_string(), timeline);
        }
        self.timelines.get_mut(channel)
    }

    /// Shows what is known about `channel` at once and reads what is
    /// missing (`ensure`).
    pub(super) fn ensure(&mut self, channel: &str) {
        let now = now_ms();
        let retained = self.channels.heads.peek(channel).cloned();
        let Some(timeline) = self.touch(channel) else {
            return;
        };
        if let Some(head) = &retained
            && timeline.idle()
        {
            timeline.apply_head(head);
        }
        timeline.publish();
        if !self.can_read_remote(channel) {
            return;
        }
        self.read_admins(channel);
        if self.demand_channel(channel) {
            return;
        }
        let timeline = self.timelines.get_mut(channel).expect("touched");
        if !timeline.idle() {
            // Revalidate only a reader still at the head: a paged-back
            // reader is never replaced because time passed.
            if retained.is_some_and(|head| !head.fresh(now))
                && timeline.at_head
                && !timeline.loading_head
            {
                self.load_head(channel);
            }
            return;
        }
        let head = self.channels.heads.get(channel).cloned();
        let timeline = self.timelines.get_mut(channel).expect("touched");
        if let Some(head) = &head {
            timeline.apply_head(head);
        }
        if head.as_ref().is_some_and(|head| head.fresh(now)) {
            timeline.publish();
            return;
        }
        if head.is_none() {
            timeline.status = super::timeline::WindowStatus::Loading;
            timeline.error = None;
        }
        timeline.publish();
        self.load_head(channel);
    }

    /// Reads the head for the window, unless one is already on its way.
    pub(super) fn load_head(&mut self, channel: &str) {
        if !self.can_read_remote(channel) {
            return;
        }
        let Some(timeline) = self.timelines.get_mut(channel) else {
            return;
        };
        if timeline.loading_head {
            return;
        }
        timeline.loading_head = true;
        let generation = timeline.generation;
        self.read_page(
            channel.to_string(),
            None,
            PageFor::Window(generation),
            Priority::Foreground,
        );
    }

    pub(super) fn load_older(&mut self, channel: &str) {
        if !self.can_read_remote(channel) {
            return;
        }
        let Some(timeline) = self.timelines.get_mut(channel) else {
            return;
        };
        if timeline.status != super::timeline::WindowStatus::Ready
            || !timeline.has_more
            || timeline.loading_older
            || timeline.history_limited
            || timeline.loading_head
        {
            return;
        }
        let Some(cursor) = timeline.cursor.clone() else {
            return;
        };
        timeline.loading_older = true;
        timeline.error = None;
        timeline.publish();
        let generation = timeline.generation;
        self.read_page(
            channel.to_string(),
            Some(cursor),
            PageFor::Window(generation),
            Priority::Foreground,
        );
    }

    /// Warms a channel's head before it opens (`prepare`): one at a time,
    /// skipped while a fresh head is retained.
    pub(super) fn prepare(&mut self, channel: &str) {
        let allowed = self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.authorized(channel));
        if !allowed || !self.can_read_remote(channel) || self.channels.preparing {
            return;
        }
        if self
            .channels
            .heads
            .get(channel)
            .is_some_and(|head| head.fresh(now_ms()))
        {
            return;
        }
        self.channels.preparing = true;
        self.read_page(
            channel.to_string(),
            None,
            PageFor::Prepare,
            Priority::Foreground,
        );
    }

    pub(super) fn read_page(
        &self,
        channel: String,
        sent: Option<Cursor>,
        purpose: PageFor,
        priority: Priority,
    ) {
        let reader = self.reader.clone();
        let commands = self.commands.clone();
        let session = self.is_session(&channel);
        let epoch = self.epoch;
        let access = self.access_version(&channel);
        self.runtime.spawn(async move {
            let options = ReadOptions {
                priority,
                fresh: false,
            };
            let result = if session {
                read_session_window(&reader, &channel, sent.as_ref(), options).await
            } else {
                reader
                    .read(vec![page_filter(&channel, sent.as_ref())], options)
                    .await
                    .map(PageRead::Window)
            };
            let _ = commands.send(Message::Page {
                channel,
                sent,
                purpose,
                epoch,
                access,
                result,
            });
        });
    }

    /// A page read finished: admit its events through the session's
    /// pipeline, then hand the page to what asked for it.
    pub(super) fn page_read(
        &mut self,
        channel: String,
        sent: Option<Cursor>,
        purpose: PageFor,
        (epoch, access): (u64, u64),
        result: Result<PageRead, ReadError>,
    ) {
        let current = epoch == self.epoch && access == self.access_version(&channel);
        if let PageFor::Window(generation) = purpose
            && let Some(timeline) = self.timelines.get_mut(&channel)
            && timeline.generation == generation
        {
            if sent.is_none() {
                timeline.loading_head = false;
            } else {
                timeline.loading_older = false;
            }
        }
        if purpose == PageFor::Prepare {
            self.channels.preparing = false;
        }
        let stale = result.is_ok() && !current;
        let page = match result {
            Ok(read) if current => self.admit_page(&channel, sent.as_ref(), read),
            Ok(_) => Err(ReadError {
                kind: ReadErrorKind::Cancelled,
                message: "Stale request".into(),
                status: None,
                retry_after: None,
            }),
            Err(error) => Err(error),
        };
        match page {
            Ok((events, cursor, has_more)) => {
                if sent.is_some() {
                    if let PageFor::Window(generation) = purpose
                        && let Some(timeline) = self.timelines.get_mut(&channel)
                        && timeline.generation == generation
                    {
                        timeline.apply_older(events, cursor, has_more);
                        timeline.publish();
                        let people = timeline.people();
                        self.ensure_profiles(people);
                    }
                } else {
                    match self.store_head(&channel, events, cursor, has_more, purpose) {
                        Ok(head) => self.head_ready(&channel, purpose, head),
                        Err(error) => self.page_failed(&channel, sent.is_some(), purpose, error),
                    }
                }
            }
            Err(error) => self.page_failed(&channel, sent.is_some(), purpose, error),
        }
        // A head discarded only for being stale (an epoch or access bump
        // since it was sent) is read again, or its window keeps loading.
        if stale
            && sent.is_none()
            && let PageFor::Window(generation) = purpose
            && self
                .timelines
                .get(&channel)
                .is_some_and(|timeline| timeline.generation == generation)
        {
            self.load_head(&channel);
        }
        self.reading_loaded(&channel);
        self.publish();
    }

    /// The page's events go through `accept` like live traffic; only what
    /// is visible becomes history.
    fn admit_page(
        &mut self,
        channel: &str,
        sent: Option<&Cursor>,
        read: PageRead,
    ) -> Result<(Vec<Verified>, Option<Cursor>, bool), ReadError> {
        let invalid = |message: String| ReadError {
            kind: ReadErrorKind::InvalidResponse,
            message,
            status: None,
            retry_after: None,
        };
        match read {
            PageRead::Window(events) => {
                let visible = self.accept(&events);
                let authority = self.relay.as_ref().map(|relay| relay.authority.clone());
                let authority = authority.ok_or_else(|| invalid("Relay is unavailable".into()))?;
                let page = parse_page(channel, &authority, sent, &visible)
                    .map_err(|error| invalid(error.to_string()))?;
                // A head keeps its signed bound, so a saved copy can be
                // checked again when restored; older pages join without it.
                let events = if sent.is_none() { visible } else { page.events };
                Ok((events, page.cursor, page.has_more))
            }
            PageRead::Session {
                events,
                cursor,
                has_more,
            } => {
                let visible = self.accept(&events);
                Ok((visible, cursor, has_more))
            }
        }
    }

    /// Builds and retains a head read from the relay (`requestHead`), and
    /// saves it to disk.
    fn store_head(
        &mut self,
        channel: &str,
        events: Vec<Verified>,
        cursor: Option<Cursor>,
        has_more: bool,
        purpose: PageFor,
    ) -> Result<Head, ReadError> {
        let relay = self.relay_key().ok_or(ReadError {
            kind: ReadErrorKind::Cancelled,
            message: "Relay is unavailable".into(),
            status: None,
            retry_after: None,
        })?;
        let rows = fold_messages(channel, &relay, &events, &self.fold_options(channel, relay));
        let bytes: usize = events.iter().map(Verified::byte_size).sum();
        if rows.len() > MAX_ROWS || bytes > MAX_BYTES {
            return Err(ReadError {
                kind: ReadErrorKind::InvalidResponse,
                message: "Channel head exceeds the read budget".into(),
                status: None,
                retry_after: None,
            });
        }
        let head = Head {
            events: events.into(),
            cursor,
            has_more,
            saved_at: now_ms(),
            cached: false,
            restored: false,
            preview: message_preview(&rows),
            people: rows_people(&rows),
        };
        let bytes = head.bytes();
        self.channels
            .heads
            .set(channel.to_string(), head.clone(), bytes);
        self.save_head(channel, &head);
        self.ensure_profiles(head.people.clone());
        if purpose == PageFor::Prepare {
            self.preload_media(&head.people);
        }
        Ok(head)
    }

    fn head_ready(&mut self, channel: &str, purpose: PageFor, head: Head) {
        match purpose {
            PageFor::Window(generation) => {
                if let Some(timeline) = self.timelines.get_mut(channel)
                    && timeline.generation == generation
                {
                    timeline.apply_head(&head);
                    timeline.publish();
                }
                self.preload_media(&head.people);
            }
            PageFor::Prepare => {}
            PageFor::CatchUp(generation) => {
                let local = self.local_ids();
                let verified = if let Some(timeline) = self.timelines.get_mut(channel) {
                    timeline.accept(&head.events, &local);
                    timeline.caught_up(&head);
                    timeline.publish();
                    true
                } else {
                    false
                };
                self.catch_up_finished(channel, generation, Ok(verified));
            }
        }
    }

    fn page_failed(&mut self, channel: &str, older: bool, purpose: PageFor, error: ReadError) {
        if error.kind == ReadErrorKind::Denied {
            self.deny_channel(channel, &error.message);
            self.check_access();
            self.update_interests();
            self.reads_channels(false);
        } else if error.kind != ReadErrorKind::Cancelled
            && let PageFor::Window(generation) = purpose
            && let Some(timeline) = self.timelines.get_mut(channel)
            && timeline.generation == generation
        {
            timeline.read_failed(error.message.clone(), older);
            timeline.publish();
        }
        if let PageFor::CatchUp(generation) = purpose {
            self.catch_up_finished(channel, generation, Err(error));
        }
    }

    // ----- catch-up after (re)establishment -----

    /// Channel routes reached EOSE: every retained window catches up with
    /// one head read; others only mark their head unconfirmed.
    pub(super) fn established_channels(&mut self, channels: &[String]) {
        let generation = self.channels.live_generation;
        for channel in channels {
            let accessible = self
                .discovery
                .as_ref()
                .is_some_and(|discovery| discovery.can_access(channel));
            if !accessible {
                continue;
            }
            self.stale_head(channel);
            let retained = self.timelines.contains_key(channel);
            self.channels.catchups.insert(
                channel.clone(),
                Catchup {
                    generation,
                    state: if retained {
                        CatchUpState::Pending
                    } else {
                        CatchUpState::Deferred
                    },
                    error: None,
                    retry_at: None,
                },
            );
            if retained && !self.channels.queue.contains(channel) {
                self.channels.queue.push_back(channel.clone());
            }
        }
        self.publish_live();
        self.catch_up_next();
    }

    /// An opened channel with a catch-up owed is loaded by that catch-up
    /// (`demandChannel`). Returns whether it was.
    pub(super) fn demand_channel(&mut self, channel: &str) -> bool {
        let generation = self.channels.live_generation;
        let Some(job) = self.channels.catchups.get_mut(channel) else {
            return false;
        };
        if job.generation != generation || job.state == CatchUpState::Verified {
            return false;
        }
        if job.retry_at.is_some_and(|at| at > Instant::now()) {
            let error = job.error.clone().unwrap_or_default();
            if let Some(timeline) = self.timelines.get_mut(channel) {
                timeline.read_failed(error, false);
                timeline.publish();
            }
            return true;
        }
        if job.state != CatchUpState::Pending {
            job.state = CatchUpState::Pending;
            job.error = None;
            if !self.channels.queue.iter().any(|id| id == channel) {
                self.channels.queue.push_back(channel.to_string());
            }
            self.publish_live();
        }
        if self.channels.running.contains(channel) {
            let channel = channel.to_string();
            self.reader
                .promote(move |filters| is_channel_head(filters, &channel));
        }
        self.catch_up_next();
        true
    }

    /// Demand order: the selected channel, then other open windows.
    fn demanded(&self) -> Vec<String> {
        let mut demanded: Vec<String> = self
            .selected
            .iter()
            .filter(|id| self.timelines.contains_key(*id))
            .cloned()
            .collect();
        let mut others: Vec<&Timeline> = self
            .timelines
            .values()
            .filter(|timeline| Some(&timeline.id) != self.selected.as_ref())
            .collect();
        others.sort_by_key(|timeline| std::cmp::Reverse(timeline.generation));
        demanded.extend(others.into_iter().map(|timeline| timeline.id.clone()));
        demanded
    }

    /// Starts queued catch-ups (`catchUpNext`): one at a time, and the
    /// selected channel may use a second slot.
    fn catch_up_next(&mut self) {
        while !self.channels.queue.is_empty() && self.channels.running.len() < CATCH_UP_SLOTS {
            let demanded = self.demanded();
            let next = if self.channels.running.is_empty() {
                demanded
                    .iter()
                    .find(|id| self.channels.queue.contains(*id))
                    .cloned()
                    .or_else(|| self.channels.queue.front().cloned())
            } else {
                demanded
                    .first()
                    .filter(|id| {
                        self.channels.queue.contains(*id) && !self.channels.running.contains(*id)
                    })
                    .cloned()
            };
            let Some(channel) = next else {
                return;
            };
            self.channels.queue.retain(|id| *id != channel);
            let generation = self.channels.live_generation;
            let current = self
                .channels
                .catchups
                .get(&channel)
                .is_some_and(|job| job.generation == generation);
            let accessible = self
                .discovery
                .as_ref()
                .is_some_and(|discovery| discovery.can_access(&channel));
            if !current || !accessible {
                continue;
            }
            self.channels.running.insert(channel.clone());
            // A head read from before the stream was established cannot
            // stand in for the catch-up.
            let fenced = channel.clone();
            self.reader
                .invalidate(move |filters| is_channel_head(filters, &fenced));
            let has_rows = self
                .timelines
                .get_mut(&channel)
                .map(|timeline| {
                    if !timeline.has_rows() {
                        timeline.status = super::timeline::WindowStatus::Loading;
                        timeline.error = None;
                        timeline.publish();
                    }
                })
                .is_some();
            if !has_rows {
                self.catch_up_finished(&channel, generation, Ok(false));
                continue;
            }
            let priority = if self.selected.as_deref() == Some(channel.as_str()) {
                Priority::Foreground
            } else {
                Priority::Background
            };
            self.read_page(channel, None, PageFor::CatchUp(generation), priority);
        }
    }

    fn catch_up_finished(
        &mut self,
        channel: &str,
        generation: u64,
        result: Result<bool, ReadError>,
    ) {
        self.channels.running.remove(channel);
        let current = self
            .channels
            .catchups
            .get(channel)
            .is_some_and(|job| job.generation == generation)
            && generation == self.channels.live_generation;
        if current {
            match result {
                Ok(verified) => {
                    let job = self.channels.catchups.get_mut(channel).expect("current");
                    job.state = if verified {
                        CatchUpState::Verified
                    } else {
                        CatchUpState::Deferred
                    };
                }
                Err(error) => {
                    let cancelled = error.kind == ReadErrorKind::Cancelled;
                    let job = self.channels.catchups.get_mut(channel).expect("current");
                    job.state = if cancelled {
                        CatchUpState::Deferred
                    } else {
                        CatchUpState::Error
                    };
                    job.error = (!cancelled).then(|| error.message.clone());
                    if !cancelled && let Some(timeline) = self.timelines.get_mut(channel) {
                        timeline.read_failed(error.message.clone(), false);
                        timeline.publish();
                    }
                    if let Some(wait) = error.retry_after {
                        // Keep the obligation, not paced requests waiting
                        // inside reader deadlines.
                        let at = Instant::now() + wait;
                        job.retry_at = Some(at);
                        let queued: Vec<String> = self.channels.queue.drain(..).collect();
                        for id in queued {
                            if let Some(job) = self.channels.catchups.get_mut(&id) {
                                job.state = CatchUpState::Error;
                                job.error = Some(error.message.clone());
                                job.retry_at = Some(at);
                            }
                            if let Some(timeline) = self.timelines.get_mut(&id) {
                                timeline.read_failed(error.message.clone(), false);
                                timeline.publish();
                            }
                        }
                    }
                }
            }
        }
        self.publish_live();
        self.catch_up_next();
    }

    pub(super) fn catch_up_states(&self) -> Vec<HeadCatchUp> {
        let mut heads: Vec<HeadCatchUp> = self
            .channels
            .catchups
            .iter()
            .map(|(channel, job)| HeadCatchUp {
                channel: channel.clone(),
                state: job.state,
                error: job.error.clone(),
            })
            .collect();
        heads.sort_by(|a, b| a.channel.cmp(&b.channel));
        heads
    }

    /// The stream dropped (`staleHeads` and the catch-up reset): heads and
    /// windows become unconfirmed and every pending read is cancelled.
    pub(super) fn channels_disconnected(&mut self) {
        self.channels.live_generation += 1;
        self.channels.catchups.clear();
        self.channels.queue.clear();
        self.channels.running.clear();
        self.channels.preparing = false;
        self.reader.invalidate(|_| true);
        let keys = self.channels.heads.keys();
        for key in keys {
            self.channels.heads.update(&key, |head| head.cached = true);
        }
        for timeline in self.timelines.values_mut() {
            timeline.loading_head = false;
            timeline.loading_older = false;
            timeline.mark_stale();
            timeline.publish();
        }
    }

    fn stale_head(&mut self, channel: &str) {
        self.channels
            .heads
            .update(channel, |head| head.cached = true);
        if let Some(timeline) = self.timelines.get_mut(channel) {
            timeline.mark_stale();
            timeline.publish();
        }
    }

    // ----- tails and previews -----

    /// Keeps the newest live events of every channel they name, for
    /// previews and for windows that open later.
    pub(super) fn accept_tails(&mut self, events: &[Verified]) {
        let Some(relay) = self.relay_key() else {
            return;
        };
        let mut channels: Vec<String> = events
            .iter()
            .flat_map(|event| event.tags_named("h").filter_map(|tag| tag.get(1).cloned()))
            .collect();
        channels.sort();
        channels.dedup();
        for channel in channels {
            let accessible = self
                .discovery
                .as_ref()
                .is_some_and(|discovery| discovery.can_access(&channel));
            if !accessible {
                continue;
            }
            let existing = self.channels.tails.remove(&channel);
            let fresh = existing.is_none();
            let mut tail = existing.unwrap_or_default();
            let mut changed = false;
            for event in events.iter().filter(|event| event.has_tag("h", &channel)) {
                changed |= tail.insert(event);
            }
            match self.timelines.get(&channel) {
                Some(timeline) => tail.preview = timeline.preview(),
                // Fold for the preview only when the tail moved.
                None if changed || fresh => {
                    let mut seen = HashSet::new();
                    let head = self.channels.heads.peek(&channel);
                    let all: Vec<Verified> = tail
                        .events
                        .iter()
                        .chain(head.iter().flat_map(|head| head.events.iter()))
                        .filter(|event| seen.insert(event.id))
                        .cloned()
                        .collect();
                    let rows =
                        fold_messages(&channel, &relay, &all, &self.fold_options(&channel, relay));
                    tail.preview = message_preview(&rows);
                }
                None => {}
            }
            let bytes = tail.bytes();
            self.channels.tails.set(channel, tail, bytes);
        }
    }

    /// A channel's sidebar preview: its window, else its tail, else its
    /// head.
    pub(super) fn preview(&self, channel: &str) -> Option<String> {
        if let Some(timeline) = self.timelines.get(channel) {
            return timeline.preview();
        }
        self.channels
            .tails
            .peek(channel)
            .and_then(|tail| tail.preview.clone())
            .or_else(|| {
                self.channels
                    .heads
                    .peek(channel)
                    .and_then(|head| head.preview.clone())
            })
    }

    /// Warms the avatars of the newest authors (`prepareMedia`).
    fn preload_media(&self, people: &[String]) {
        let profiles = self.profiles.snapshot();
        let mut seen = HashSet::new();
        let urls: Vec<String> = people
            .iter()
            .rev()
            .filter(|id| seen.insert(*id))
            .take(12)
            .filter_map(|id| profiles.get(id).and_then(|profile| profile.picture.clone()))
            .collect();
        if !urls.is_empty() {
            self.media.prefetch_avatars(urls);
        }
    }

    // ----- access -----

    /// Access to one channel is gone (`denyChannel`): drop everything kept
    /// for it, here and on disk.
    pub(super) fn deny_channel(&mut self, channel: &str, error: &str) {
        self.bump_access(channel);
        if let Some(discovery) = &mut self.discovery {
            discovery.deny(channel);
        }
        self.channels.heads.remove(channel);
        self.channels.tails.remove(channel);
        self.next_generation += 1;
        let generation = self.next_generation;
        if let Some(timeline) = self.timelines.get_mut(channel) {
            timeline.deny(error.to_string(), generation);
            timeline.publish();
        }
        self.remove_saved_head(channel);
        self.save_startup();
    }

    /// Forgets one channel's head, tail and saved copy.
    pub(super) fn deny_cached(&mut self, channel: &str) {
        self.channels.heads.remove(channel);
        self.channels.tails.remove(channel);
        self.channels.admins.remove(channel);
        self.remove_saved_head(channel);
    }

    /// Re-filters everything retained after an access change
    /// (`purgeAccess`): heads, tails and windows keep only what is still
    /// visible, and saved heads are dropped.
    pub(super) fn purge_retained(&mut self) {
        let relay = self.relay_key();
        let accessible = |discovery: &Option<DiscoveryState>, id: &str| {
            discovery.as_ref().is_some_and(|d| d.can_access(id))
        };
        for key in self.channels.heads.keys() {
            if !accessible(&self.discovery, &key) {
                self.channels.heads.remove(&key);
                continue;
            }
            let head = self.channels.heads.peek(&key).expect("listed").clone();
            let events = self.visible(&head.events, false);
            if events.len() != head.events.len()
                && let Some(relay) = relay
            {
                let rows = fold_messages(&key, &relay, &events, &self.fold_options(&key, relay));
                let next = Head {
                    events: events.into(),
                    preview: message_preview(&rows),
                    people: rows_people(&rows),
                    ..head
                };
                let bytes = next.bytes();
                self.channels.heads.set(key, next, bytes);
            }
        }
        for key in self.channels.tails.keys() {
            if !accessible(&self.discovery, &key) {
                self.channels.tails.remove(&key);
                continue;
            }
            let events = self
                .channels
                .tails
                .peek(&key)
                .expect("listed")
                .events
                .clone();
            let visible = self.visible(&events, false);
            if visible.len() != events.len() {
                self.channels
                    .tails
                    .update(&key, |tail| tail.events = visible);
            }
        }
        for key in self.channels.admins.keys() {
            if !accessible(&self.discovery, &key) {
                self.channels.admins.remove(&key);
            }
        }
        let ids: Vec<String> = self.timelines.keys().cloned().collect();
        for id in ids {
            self.next_generation += 1;
            let generation = self.next_generation;
            if !accessible(&self.discovery, &id) {
                continue;
            }
            let all = self.timelines[&id].events();
            let visible: HashSet<EventId> =
                self.visible(&all, false).iter().map(|e| e.id).collect();
            if let Some(timeline) = self.timelines.get_mut(&id) {
                timeline.purge(
                    |events| {
                        events
                            .iter()
                            .filter(|e| visible.contains(&e.id))
                            .cloned()
                            .collect()
                    },
                    generation,
                );
                timeline.publish();
            }
        }
        // Saved heads also carry profiles and other channels' auxiliary
        // evidence: drop them all rather than reason about each.
        if self.channels.hydrated {
            self.retain_saved_heads(HashSet::new());
        }
    }

    // ----- the device cache -----

    /// Saves a head read from the relay with its authors' profiles. Never
    /// session channels (no signed bounds) or channels not joined.
    fn save_head(&mut self, channel: &str, head: &Head) {
        let joined = self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.authorized(channel));
        if !joined || self.is_session(channel) {
            return;
        }
        let mut seen = HashSet::new();
        let profiles: Vec<Verified> = head
            .people
            .iter()
            .filter(|id| seen.insert(*id))
            .filter_map(|id| self.profiles.event(id).cloned())
            .collect();
        let (store, partition) = (self.store.clone(), self.partition());
        let (channel, head) = (channel.to_string(), head.clone());
        self.disk.write(move || {
            let saved = store.save_head(
                &partition,
                &channel,
                head.saved_at as i64,
                &head.events,
                &profiles,
                now_ms() as i64,
            );
            if let Err(error) = saved {
                tracing::debug!(%error, "could not save a channel head");
            }
        });
        self.channels.hydrated = true;
    }

    fn remove_saved_head(&self, channel: &str) {
        let (store, partition, channel) =
            (self.store.clone(), self.partition(), channel.to_string());
        self.disk.write(move || {
            let _ = store.remove_head(&partition, &channel);
        });
    }

    pub(super) fn retain_saved_heads(&self, channels: HashSet<String>) {
        let (store, partition) = (self.store.clone(), self.partition());
        self.disk.write(move || {
            let _ = store.retain_heads(&partition, &channels);
        });
    }

    /// Saves the confirmed roster and its participants' profiles
    /// (`saveDiscovery`), one write at a time.
    pub(super) fn save_startup(&mut self) {
        let Some(discovery) = &self.discovery else {
            return;
        };
        let Some(relay) = &self.relay else {
            return;
        };
        if self.channels.startup_writing {
            self.channels.startup_dirty = true;
            return;
        }
        let events: Vec<Value> = discovery
            .saved_events()
            .iter()
            .filter_map(|event| serde_json::to_value(event.event()).ok())
            .collect();
        let mut participants: Vec<String> = discovery
            .channels()
            .into_iter()
            .filter(|channel| !channel.cached)
            .flat_map(|channel| channel.participants.unwrap_or_default())
            .collect();
        participants.sort();
        participants.dedup();
        participants.truncate(MAX_SAVED_PROFILES);
        let profiles: Vec<Value> = participants
            .iter()
            .filter_map(|id| self.profiles.event(id))
            .filter_map(|event| serde_json::to_value(event.event()).ok())
            .collect();
        let record = json!({
            "savedAt": now_ms(),
            "relayAuthor": relay.authority,
            "events": events,
            "profiles": profiles,
        })
        .to_string();
        self.channels.startup_writing = true;
        self.channels.startup_dirty = false;
        let (store, partition, commands) =
            (self.store.clone(), self.partition(), self.commands.clone());
        self.disk.write(move || {
            if let Err(error) = store.save_startup_discovery(&partition, &record, now_ms() as i64) {
                tracing::debug!(%error, "could not save the roster");
            }
            let _ = commands.send(Message::StartupSaved);
        });
    }

    pub(super) fn startup_saved(&mut self) {
        self.channels.startup_writing = false;
        if std::mem::take(&mut self.channels.startup_dirty) {
            self.save_startup();
        }
    }

    /// Reads the device cache off the actor; the session connects once it
    /// is applied, so a saved snapshot always shows first.
    pub(super) fn begin_restore(&mut self) {
        self.restoring = true;
        let partition = self.partition();
        self.channels.initial = self
            .store
            .view_state(&partition, "selected-channel")
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok());
        let (store, verifier, commands) = (
            self.store.clone(),
            self.verifier.clone(),
            self.commands.clone(),
        );
        self.runtime.spawn_blocking(move || {
            let restored = restore(&store, &partition, &verifier);
            let _ = commands.send(Message::Restored(restored.map(Box::new)));
        });
    }

    /// Shows the saved roster and heads (`restoreLocal`): display only,
    /// marked cached, never authority to write. Then connects.
    pub(super) fn restored(&mut self, restored: Option<Restored>) {
        self.restoring = false;
        if let Some(restored) = restored
            && self.discovery.is_none()
        {
            let mut discovery =
                DiscoveryState::new(self.identity.pubkey_hex(), restored.author.clone());
            discovery.apply(&restored.roster, None, None, true);
            if !discovery.channels().is_empty() {
                let participants: HashSet<String> = discovery
                    .channels()
                    .into_iter()
                    .flat_map(|channel| channel.participants.unwrap_or_default())
                    .collect();
                let profiles: Vec<Verified> = restored
                    .profiles
                    .into_iter()
                    .filter(|event| {
                        event.kind_u16() == kinds::PROFILE
                            && participants.contains(&event.pubkey_hex())
                    })
                    .collect();
                self.profiles.accept(&profiles);
                self.access_revision = discovery.access_revision();
                self.discovery = Some(discovery);
                self.remember_known();
                self.channels.cached_author = Some(restored.author);
                self.cached = true;
                self.hydrate(restored.heads);
            }
        }
        self.publish();
        self.connect();
    }

    /// Restores saved heads (`hydrate`): the last open channel first, only
    /// joined non-session channels, each re-checked against its signed
    /// bound.
    fn hydrate(&mut self, mut heads: Vec<RestoredHead>) {
        let Some(author) = self.channels.cached_author.clone() else {
            return;
        };
        let now = now_ms();
        if let Some(initial) = &self.channels.initial
            && let Some(index) = heads.iter().position(|head| &head.channel == initial)
        {
            let first = heads.remove(index);
            heads.insert(0, first);
        }
        let allowed: HashSet<String> = self
            .discovery
            .as_ref()
            .map(|discovery| discovery.channels().into_iter().map(|c| c.id).collect())
            .unwrap_or_default();
        for saved in heads {
            let channel = saved.channel.clone();
            if !allowed.contains(&channel)
                || self.is_session(&channel)
                || self.channels.heads.contains(&channel)
                || saved.saved_ms > now
                || now - saved.saved_ms > MAX_SAVED_AGE_MS as u64
            {
                continue;
            }
            let events = self.visible(&saved.events, false);
            let Ok(page) = parse_page(&channel, &author, None, &events) else {
                continue;
            };
            let Ok(relay) = PublicKey::from_hex(&author) else {
                return;
            };
            self.profiles.accept(&saved.profiles);
            let rows = fold_messages(
                &channel,
                &relay,
                &page.events,
                &self.fold_options(&channel, relay),
            );
            let head = Head {
                events: events.into(),
                cursor: page.cursor,
                has_more: page.has_more,
                saved_at: saved.saved_ms,
                cached: true,
                restored: true,
                preview: message_preview(&rows),
                people: rows_people(&rows),
            };
            let bytes = head.bytes();
            self.channels
                .heads
                .set(channel.clone(), head.clone(), bytes);
            if let Some(timeline) = self.timelines.get_mut(&channel)
                && timeline.idle()
            {
                timeline.apply_head(&head);
                timeline.publish();
            }
        }
        self.channels.hydrated = true;
    }

    /// NIP-11 named the relay's identity: a saved snapshot under another
    /// identity is dropped, never trusted.
    pub(super) fn confirm_cached_author(&mut self, authority: &str) {
        let Some(author) = self.channels.cached_author.take() else {
            return;
        };
        if author == authority {
            return;
        }
        self.discovery = None;
        self.channels.heads.clear();
        self.channels.tails.clear();
        let ids: Vec<String> = self.timelines.keys().cloned().collect();
        for id in ids {
            self.next_generation += 1;
            let generation = self.next_generation;
            if let Some(timeline) = self.timelines.get_mut(&id) {
                timeline.purge(|_| Vec::new(), generation);
                timeline.publish();
            }
        }
    }

    /// Forgets everything cached in memory and on disk (`clearCache`);
    /// open windows load again.
    pub(super) fn clear_cache(&mut self) -> tokio::sync::oneshot::Receiver<()> {
        self.epoch += 1;
        self.workflows.clear();
        self.channels.catchups.clear();
        self.channels.queue.clear();
        self.channels.running.clear();
        self.channels.preparing = false;
        self.channels.heads.clear();
        self.channels.tails.clear();
        self.channels.hydrated = false;
        self.reader.invalidate(|_| true);
        self.recent.clear();
        self.typing.clear();
        self.profiles.clear();
        self.emoji_clear();
        self.statuses_clear();
        self.members_clear();
        self.activity.values.clear();
        self.activity_cancel();
        if let Some(discovery) = &mut self.discovery {
            discovery.clear_cached();
        }
        if let Some(reads) = &mut self.reads {
            reads.unread.clear();
        }
        let ids: Vec<String> = self.timelines.keys().cloned().collect();
        for id in &ids {
            self.next_generation += 1;
            let generation = self.next_generation;
            if let Some(timeline) = self.timelines.get_mut(id) {
                timeline.purge(|_| Vec::new(), generation);
                timeline.publish();
            }
        }
        for id in ids {
            if self.timelines.get(&id).is_some_and(|t| t.viewers > 0) {
                self.ensure(&id);
            }
        }
        self.update_interests();
        self.publish();
        let (store, partition) = (self.store.clone(), self.partition());
        self.disk.write(move || {
            if let Err(error) = store.clear_cache(&partition) {
                tracing::warn!(%error, "could not clear the device cache");
            }
        });
        self.disk.flushed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpError;
    use crate::reader::{ReaderOptions, Source};
    use crate::testing::{CHANNEL, Signer, T0};
    use futures::future::BoxFuture;
    use std::sync::Mutex;

    /// Serves the session-window reads: rows by `#h`, overlays by `#e`.
    struct Fake {
        rows: Vec<Verified>,
        overlays: Vec<Verified>,
        reads: Mutex<Vec<Filter>>,
    }

    impl Source for Fake {
        fn query(
            &self,
            filters: Vec<Filter>,
            _priority: Priority,
        ) -> BoxFuture<'static, Result<Vec<Value>, HttpError>> {
            let filter = filters[0].clone();
            self.reads.lock().unwrap().push(filter.clone());
            let wanted = |event: &Verified| -> bool {
                let until = filter.get("until").and_then(Value::as_u64);
                until.is_none_or(|until| event.created_at_secs() < until)
            };
            let events: Vec<Value> = if let Some(targets) = filter.get("#e") {
                let targets: Vec<&str> = targets
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                self.overlays
                    .iter()
                    .filter(|event| {
                        event
                            .tags_named("e")
                            .any(|tag| tag.get(1).is_some_and(|id| targets.contains(&id.as_str())))
                    })
                    .filter(|event| wanted(event))
                    .map(|event| serde_json::to_value(event.event()).unwrap())
                    .collect()
            } else {
                self.rows
                    .iter()
                    .filter(|event| wanted(event))
                    .map(|event| serde_json::to_value(event.event()).unwrap())
                    .collect()
            };
            Box::pin(async move { Ok(events) })
        }
    }

    #[test]
    fn tails_keep_the_newest_in_order() {
        let signer = Signer::new();
        let event = |at: u64| signer.sign(&signer.viewer, kinds::CHAT, at, "x", &[&["h", CHANNEL]]);
        let mut tail = Tail::default();
        for at in 0..TAIL_EVENTS as u64 + 10 {
            assert!(tail.insert(&event(T0 + at)), "newer than everything kept");
        }
        assert_eq!(tail.events.len(), TAIL_EVENTS);
        let newest = event(T0 + 1_000);
        assert!(tail.insert(&newest));
        assert!(!tail.insert(&newest), "already kept");
        assert_eq!(tail.events.len(), TAIL_EVENTS);
        assert_eq!(tail.events[0].id, newest.id);
        assert!(!tail.insert(&event(T0)), "older than everything kept");
        assert!(
            tail.events
                .windows(2)
                .all(|pair| pair[0].created_at > pair[1].created_at
                    || (pair[0].created_at == pair[1].created_at && pair[0].id < pair[1].id))
        );
    }

    #[tokio::test]
    async fn session_channels_page_flat_with_every_overlay() {
        let signer = Signer::new();
        let row = signer.sign(&signer.viewer, kinds::CHAT, T0, "hi", &[&["h", CHANNEL]]);
        let reply = signer.sign(
            &signer.viewer,
            kinds::CHAT,
            T0 + 1,
            "reply",
            &[&["h", CHANNEL], &["e", &row.id_hex(), "", "root"]],
        );
        let reaction = signer.sign(
            &signer.viewer,
            kinds::REACTION,
            T0 + 2,
            "+",
            &[&["e", &row.id_hex()]],
        );
        let undo = signer.sign(
            &signer.viewer,
            kinds::DELETION,
            T0 + 3,
            "",
            &[&["e", &reaction.id_hex()]],
        );
        let fake = Arc::new(Fake {
            rows: vec![row.clone(), reply.clone()],
            overlays: vec![reaction.clone(), undo.clone()],
            reads: Mutex::default(),
        });
        let reader = Reader::spawn(
            &tokio::runtime::Handle::current(),
            fake.clone(),
            Arc::new(Verifier::default()),
            ReaderOptions::default(),
        );
        let page = read_session_window(&reader, CHANNEL, None, ReadOptions::default())
            .await
            .unwrap();
        let PageRead::Session {
            events,
            cursor,
            has_more,
        } = page
        else {
            panic!("a session page");
        };
        let ids: HashSet<EventId> = events.iter().map(|e| e.id).collect();
        // Replies are rows here; the reaction and its deletion came along.
        for event in [&row, &reply, &reaction, &undo] {
            assert!(ids.contains(&event.id));
        }
        assert!(has_more, "general queries page until an empty page");
        let cursor = cursor.unwrap();
        assert_eq!(cursor.id, row.id_hex(), "the oldest row");
        let reads = fake.reads.lock().unwrap();
        assert!(
            reads[0].get("top_level").is_none(),
            "no signed window bound"
        );
        assert_eq!(reads[0]["limit"], json!(PAGE_SIZE));
    }

    #[test]
    fn a_catch_up_replaces_only_the_channel_head() {
        let head = page_filter(CHANNEL, None);
        assert!(is_channel_head(&[head], CHANNEL));
        assert!(!is_channel_head(&[page_filter("other", None)], CHANNEL));
        let older = page_filter(
            CHANNEL,
            Some(&Cursor {
                created_at: T0,
                id: "0".repeat(64),
            }),
        );
        assert!(!is_channel_head(&[older], CHANNEL));
    }

    fn partition() -> Partition {
        Partition {
            origin: "https://relay.example".into(),
            viewer: "ab".repeat(32),
        }
    }

    fn snapshot(signer: &Signer, saved_at: u64, events: &[Verified]) -> String {
        json!({
            "savedAt": saved_at,
            "relayAuthor": signer.relay_hex(),
            "events": events.iter().map(|e| serde_json::to_value(e.event()).unwrap()).collect::<Vec<_>>(),
            "profiles": [],
        })
        .to_string()
    }

    #[test]
    fn restores_only_a_fresh_signed_snapshot() {
        let signer = Signer::new();
        let store = Store::in_memory().unwrap();
        let verifier = Verifier::default();
        let roster = signer.relay(
            kinds::CHANNEL_MEMBERS,
            T0,
            &[&["d", CHANNEL], &["p", &signer.viewer_hex()]],
        );
        let now = now_ms();
        assert!(
            restore(&store, &partition(), &verifier).is_none(),
            "nothing saved"
        );
        store
            .save_startup_discovery(
                &partition(),
                &snapshot(&signer, now, std::slice::from_ref(&roster)),
                now as i64,
            )
            .unwrap();
        let message = signer.sign(&signer.viewer, kinds::CHAT, T0, "hi", &[&["h", CHANNEL]]);
        store
            .save_head(
                &partition(),
                CHANNEL,
                now as i64,
                &[message],
                &[],
                now as i64,
            )
            .unwrap();
        let restored = restore(&store, &partition(), &verifier).unwrap();
        assert_eq!(restored.author, signer.relay_hex());
        assert_eq!(restored.roster.len(), 1);
        assert_eq!(restored.heads.len(), 1);
        assert_eq!(restored.heads[0].channel, CHANNEL);

        // Older than a day: unused.
        let stale = now - MAX_SAVED_AGE_MS as u64 - 1;
        store
            .save_startup_discovery(
                &partition(),
                &snapshot(&signer, stale, std::slice::from_ref(&roster)),
                now as i64,
            )
            .unwrap();
        assert!(restore(&store, &partition(), &verifier).is_none());

        // One tampered roster event discards the snapshot.
        let mut tampered: Value = serde_json::from_str(&snapshot(&signer, now, &[roster])).unwrap();
        tampered["events"][0]["content"] = json!("changed");
        store
            .save_startup_discovery(&partition(), &tampered.to_string(), now as i64)
            .unwrap();
        assert!(restore(&store, &partition(), &verifier).is_none());
    }
}
