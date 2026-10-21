//! Channel discovery: which channels the viewer belongs to, which they may
//! read, and what the sidebar shows about each one (`docs/port/relay.md`
//! §5a.2–5a.3, §8.2, §8.4).
//!
//! Everything here is synchronous state. The session runs the reads these
//! types ask for and feeds the verified results back:
//!
//! - [`DiscoveryState`] keeps the newest relay-signed 39000 metadata and
//!   39002 roster per channel and answers the access questions
//!   (`authorized`, `can_access`, `can_participate`).
//! - [`RosterScan`] drives one full roster pass: viewer-scoped 39002 pages,
//!   exact confirmation of memberships the pages omitted, then 39000
//!   metadata for channels that still lack names.
//! - [`ChannelSummary`] is the per-channel projection the sidebar consumes.
//!
//! Ported from `src/features/relay/discovery.ts`, the discovery parts of
//! `store.ts` (1015-1411) and `membership.ts`.

use crate::event::{Verified, first_tag as tag, is_hex64, is_newer, kinds};
use crate::socket::{Filter, filter};
use nostr::prelude::{Event, EventId};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{self, AtomicU64};

/// Relay page size for roster and metadata reads (`store.ts:80`).
pub const DISCOVERY_LIMIT: usize = 500;
/// Largest exact `#d` confirmation batch the relay accepts (`store.ts:91`).
pub const DISCOVERY_CONFIRM_LIMIT: usize = 128;
/// Retained rosters and retained metadata, each (`discovery.ts:37`).
pub const DISCOVERY_CAPACITY: usize = 1024;
/// Largest exact channel resolution (`store.ts` `resolve`).
pub const RESOLVE_LIMIT: usize = 128;

/// The `about` text that marks a private stream as a work session
/// (`src/features/sessions/metadata.ts`).
pub const SESSION_CHANNEL_DESCRIPTION: &str = "Buzz session (buzz.sessions/v1)";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    /// The relay answered with something a correct relay cannot send. The
    /// session reports it like any `invalid-response` read error.
    #[error("{0}")]
    InvalidResponse(&'static str),
    #[error("Too many search result channels")]
    TooManyChannels,
}

/// Explicit signed visibility. Missing or contradictory flags are unknown,
/// never implicitly public.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Visibility {
    Public,
    Private,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelType {
    Stream,
    Forum,
    Dm,
    /// A private stream whose `about` is session metadata.
    Session,
}

/// One channel as the sidebar and headers see it (`contracts.ts:16-45`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelSummary {
    pub id: String,
    /// Metadata name, or the first 8 characters of the id. The sidebar
    /// replaces DM names with participant labels (see `sidebar`).
    pub name: String,
    /// Ordinary channel prose. `None` without metadata and for sessions.
    pub description: Option<String>,
    pub visibility: Option<Visibility>,
    /// Newest activity for "recent" sorting, in Unix seconds. Discovery
    /// leaves it empty; the channel-activity projection fills it.
    pub last_activity_at: Option<u64>,
    /// Readable but not writable: a public preview the viewer has not
    /// joined, or membership restored from disk and not yet confirmed.
    pub read_only: bool,
    /// Membership restored from disk, display only.
    pub cached: bool,
    /// NIP-29 `hidden` (members-only, absent from directories), e.g. DMs.
    pub hidden: bool,
    pub private: bool,
    pub archived: bool,
    pub channel_type: Option<ChannelType>,
    /// Presentation-only parent of a session; never grants access.
    pub parent_channel_id: Option<String>,
    /// Metadata time of a session, for stable work-history ordering.
    pub updated_at: Option<u64>,
    /// Sorted, unique members from the relay-signed roster.
    pub members: Vec<String>,
    /// DM only: members other than the viewer. Empty for notes to self.
    pub participants: Option<Vec<String>>,
    /// The newest conversational message, for the sidebar
    /// (`messagePreview`). Filled by the session from its windows, live
    /// tails and heads; discovery leaves it empty.
    pub preview: Option<String>,
    /// Relay-published NIP-CL labels: grouping, never trust.
    pub labels: Vec<String>,
    /// Relay-written creator (NIP-CL `P`), the 9007 signer. Ownership can
    /// move later.
    pub creator: Option<String>,
    /// Relay-written creation time in Unix seconds.
    pub created_at: Option<u64>,
}

/// Which version of each channel's roster a pass started from, by event id.
pub type RosterVersions = HashMap<String, EventId>;

/// The `d` tag, as discovery uses it: present and non-empty.
fn channel_id(event: &Event) -> Option<&str> {
    tag(event, "d").filter(|id| !id.is_empty())
}

fn has_tag(event: &Event, name: &str, value: &str) -> bool {
    event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.first().map(String::as_str) == Some(name)
            && slice.get(1).map(String::as_str) == Some(value)
    })
}

fn has_tag_named(event: &Event, name: &str) -> bool {
    event.tags.iter().any(|tag| tag.kind() == name)
}

/// Lowercase hyphenated UUID, the shape of channel ids.
pub fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => matches!(b, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// Approximates JavaScript's `String.prototype.localeCompare` in the root
/// locale for the names and ids the sidebar sorts: case-insensitive first,
/// then lowercase before uppercase. Rust has no ICU collation here, so
/// punctuation and accents order by code point instead.
pub(crate) fn locale_compare(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| b.cmp(a))
}

/// Display name from signed metadata: a JSON content `name` wins over the
/// `name` tag (`discovery.ts:8-12`).
pub fn metadata_name(event: &Event) -> Option<String> {
    if let Ok(Value::Object(body)) = serde_json::from_str::<Value>(&event.content)
        && let Some(Value::String(name)) = body.get("name")
        && !name.is_empty()
    {
        return Some(name.clone());
    }
    tag(event, "name")
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Explicit signed public metadata: readable by any viewer, but never
/// membership (`discovery.ts:15-21`).
pub fn open_metadata(event: &Event) -> bool {
    has_tag_named(event, "public")
        && !has_tag_named(event, "private")
        && !has_tag_named(event, "hidden")
        && !has_tag(event, "t", "dm")
}

/// Exactly one single-element `public` or `private` tag, else unknown
/// (`channel-details-protocol.ts:26-36`).
pub fn channel_visibility(event: &Event) -> Option<Visibility> {
    let mut flags = event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| matches!(tag.first().map(String::as_str), Some("public" | "private")));
    match (flags.next(), flags.next()) {
        (Some(flag), None) if flag.len() == 1 => match flag[0].as_str() {
            "public" => Some(Visibility::Public),
            _ => Some(Visibility::Private),
        },
        _ => None,
    }
}

/// Session metadata parsed from a channel `about`: `Some(None)` for a
/// top-level session, `Some(Some(parent))` for a child session.
pub fn session_metadata(about: Option<&str>) -> Option<Option<String>> {
    let about = about?;
    if about == SESSION_CHANNEL_DESCRIPTION {
        return Some(None);
    }
    let parent = about
        .strip_prefix(SESSION_CHANNEL_DESCRIPTION)?
        .strip_prefix("\nparent:")?;
    is_uuid(parent).then(|| Some(parent.to_owned()))
}

fn roster_members<'a>(roster: Option<&'a Verified>) -> impl Iterator<Item = &'a str> + 'a {
    roster.into_iter().flat_map(|roster| {
        roster
            .tags_named("p")
            .filter_map(|tag| tag.get(1))
            .map(String::as_str)
            .filter(|value| is_hex64(value))
    })
}

fn sorted_unique<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut values: Vec<String> = values.map(str::to_owned).collect();
    values.sort();
    values.dedup();
    values
}

/// What one [`DiscoveryState::apply`] call did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Some retained version changed.
    pub changed: bool,
    /// An event was dropped because retention is full. Completeness can no
    /// longer be certified by this pass.
    pub overflowed: bool,
    /// A complete roster set was applied: omission now proves loss.
    pub certified: bool,
    /// Some channel became inaccessible. The session revokes access.
    pub access_changed: bool,
}

/// Source of [`DiscoveryState::revision`]. One counter for every state, so
/// a state that replaces another never repeats a revision it handed out.
static REVISIONS: AtomicU64 = AtomicU64::new(1);

fn next_revision() -> u64 {
    REVISIONS.fetch_add(1, atomic::Ordering::Relaxed)
}

/// NIP-29 discovery state for one viewer in one community
/// (`discovery.ts:24-296`, plus the bookkeeping of `store.ts`
/// `applyDiscovery`).
#[derive(Debug)]
pub struct DiscoveryState {
    viewer: String,
    relay_author: String,
    capacity: usize,
    denied: HashSet<String>,
    suspended: HashSet<String>,
    complete: bool,
    cached: HashSet<String>,
    /// Channels whose current metadata came from disk and must be re-read.
    pending_metadata: HashSet<String>,
    /// Changes with anything the state holds; see [`Self::revision`].
    revision: u64,
    access_revision: u64,
    overflow_revision: u64,
    rosters: BTreeMap<String, Verified>,
    metadata: BTreeMap<String, Verified>,
}

impl DiscoveryState {
    /// `get(id).is_some_and(|summary| summary.cached)`, without building
    /// the summary.
    pub fn is_cached(&self, id: &str) -> bool {
        self.cached.contains(id) && self.preview(id).is_some()
    }
}

impl DiscoveryState {
    /// `relay_author` is the community's NIP-11 `self` (§8.2).
    pub fn new(viewer: impl Into<String>, relay_author: impl Into<String>) -> Self {
        Self::with_capacity(viewer, relay_author, DISCOVERY_CAPACITY)
    }

    pub fn with_capacity(
        viewer: impl Into<String>,
        relay_author: impl Into<String>,
        capacity: usize,
    ) -> Self {
        Self {
            viewer: viewer.into(),
            relay_author: relay_author.into(),
            capacity,
            denied: HashSet::new(),
            suspended: HashSet::new(),
            complete: false,
            cached: HashSet::new(),
            pending_metadata: HashSet::new(),
            revision: next_revision(),
            access_revision: 0,
            overflow_revision: 0,
            rosters: BTreeMap::new(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn viewer(&self) -> &str {
        &self.viewer
    }

    pub fn relay_author(&self) -> &str {
        &self.relay_author
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Changes whenever anything the state holds changes: summaries,
    /// [`Self::channels`], access or participation. Unique across states,
    /// so it keys caches of what was derived from it.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn touch(&mut self) {
        self.revision = next_revision();
    }

    /// Bumped whenever some channel stops being accessible.
    pub fn access_revision(&self) -> u64 {
        self.access_revision
    }

    /// Bumped whenever retention drops an event.
    pub fn overflow_revision(&self) -> u64 {
        self.overflow_revision
    }

    /// A complete roster read (or a cache restore) closed the unknown
    /// boundary: channels without evidence are no longer accessible.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Takes one relay-signed 39000 or 39002. Other kinds and other authors
    /// are ignored, not trusted (§8.2). `cached` marks events restored from
    /// disk. Returns whether visible state changed.
    pub fn accept(&mut self, event: &Verified, cached: bool) -> bool {
        let kind = event.kind_u16();
        if event.pubkey_hex() != self.relay_author
            || (kind != kinds::CHANNEL_METADATA && kind != kinds::CHANNEL_MEMBERS)
        {
            return false;
        }
        let Some(id) = channel_id(event) else {
            return false;
        };
        let is_roster = kind == kinds::CHANNEL_MEMBERS;
        let map = if is_roster {
            &self.rosters
        } else {
            &self.metadata
        };
        if !map.contains_key(id) && map.len() >= self.capacity {
            self.overflow_revision += 1;
            return false;
        }
        let previous = map.get(id).cloned();
        let replaces = previous
            .as_ref()
            .is_none_or(|previous| is_newer(event, previous));
        // Only this version or a newer one confirms saved membership. An
        // older response must not promote a newer disk roster into write
        // authority.
        let confirmed = is_roster
            && !cached
            && (replaces || previous.as_ref().is_some_and(|p| p.id == event.id))
            && self.cached.remove(id);
        if !replaces {
            if confirmed {
                self.touch();
            }
            return confirmed;
        }
        self.touch();
        let id = id.to_owned();
        let accessible = self.can_access(&id);
        if is_roster {
            self.rosters.insert(id.clone(), event.clone());
            if cached {
                self.cached.insert(id.clone());
            }
            if previous
                .as_ref()
                .is_some_and(|previous| has_tag(previous, "p", &self.viewer))
                && !has_tag(event, "p", &self.viewer)
            {
                self.denied.insert(id.clone());
            } else if has_tag(event, "p", &self.viewer) {
                self.denied.remove(&id);
            }
        } else {
            self.metadata.insert(id.clone(), event.clone());
            if previous.is_some() && !self.rosters.contains_key(&id) && self.open(&id) {
                self.denied.remove(&id);
            }
        }
        if self.authorized(&id) || (!is_roster && self.open(&id)) {
            self.suspended.remove(&id);
        }
        if accessible && !self.can_access(&id) {
            self.access_revision += 1;
        }
        true
    }

    /// Applies a verified batch, as `store.ts` `applyDiscovery` does.
    ///
    /// `complete` is the full set of channel ids a complete viewer-scoped
    /// roster read returned; only such a set proves that omitted
    /// memberships were lost. `started` is the roster versions the read
    /// started from (defaults to the versions before this batch). Capped
    /// reads and live traffic pass `None` and never revoke by omission.
    pub fn apply(
        &mut self,
        events: &[Verified],
        complete: Option<&HashSet<String>>,
        started: Option<&RosterVersions>,
        cached: bool,
    ) -> Applied {
        let default_started;
        let started = match started {
            Some(started) => started,
            None => {
                default_started = self.roster_versions();
                &default_started
            }
        };
        let access_revision = self.access_revision;
        let overflow_revision = self.overflow_revision;
        if cached {
            self.restrict_to_known();
        }
        let mut changed = false;
        for event in events {
            changed |= self.accept(event, cached);
            if event.kind_u16() == kinds::CHANNEL_METADATA
                && let Some(id) = channel_id(event)
                && self.metadata.get(id).is_some_and(|m| m.id == event.id)
            {
                let pending = if cached {
                    self.pending_metadata.insert(id.to_owned())
                } else {
                    self.pending_metadata.remove(id)
                };
                if pending {
                    self.touch();
                }
            }
        }
        let overflowed = self.overflow_revision != overflow_revision;
        let complete = if overflowed { None } else { complete };
        let certified = match complete {
            Some(complete) => {
                changed |= self.retain(complete, started);
                true
            }
            None => false,
        };
        Applied {
            changed,
            overflowed,
            certified,
            access_changed: self.access_revision != access_revision,
        }
    }

    /// Explicit loss of access (CLOSED "not a channel member", HTTP 401/403
    /// on a channel read, and so on; §8.4). Denials are never evicted back
    /// to "unknown": when the denied set is full, everything is denied
    /// until fresh signed membership arrives.
    pub fn deny(&mut self, id: &str) {
        if !self.can_access(id) && !self.suspended.contains(id) {
            return;
        }
        if self.denied.len() >= self.capacity {
            self.deny_all();
            return;
        }
        self.touch();
        self.access_revision += 1;
        self.suspended.remove(id);
        self.denied.insert(id.to_owned());
    }

    /// A CLOSED public preview is unreadable until fresh evidence arrives,
    /// but not permanently denied. Only read-only previews can be
    /// suspended.
    pub fn suspend(&mut self, id: &str) -> bool {
        if self.preview(id) != Some(true) {
            return false;
        }
        self.suspended.insert(id.to_owned());
        self.touch();
        self.access_revision += 1;
        true
    }

    pub fn suspended_channels(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.suspended.iter().cloned().collect();
        ids.sort();
        ids
    }

    pub fn resume(&mut self, id: &str) -> bool {
        let resumed = self.suspended.remove(id);
        if resumed {
            self.touch();
        }
        resumed
    }

    /// The current roster version of every retained channel.
    pub fn roster_versions(&self) -> RosterVersions {
        self.rosters
            .iter()
            .map(|(id, roster)| (id.clone(), roster.id))
            .collect()
    }

    pub fn roster(&self, id: &str) -> Option<&Verified> {
        self.rosters.get(id)
    }

    /// Records that a complete viewer-scoped roster read returned exactly
    /// `ids`. Rosters that listed the viewer, were omitted, and have not
    /// changed since `started` are denied. Returns whether anything changed.
    pub fn retain(&mut self, ids: &HashSet<String>, started: &RosterVersions) -> bool {
        let mut changed = !self.complete;
        self.complete = true;
        // Keep the last signed version: an old replay cannot undo an
        // authoritative loss.
        for (id, roster) in &self.rosters {
            if has_tag(roster, "p", &self.viewer)
                && !ids.contains(id)
                && !self.denied.contains(id)
                && started.get(id) == Some(&roster.id)
            {
                self.denied.insert(id.clone());
                changed = true;
            }
        }
        if changed {
            self.touch();
            self.access_revision += 1;
        }
        changed
    }

    /// The roster list itself was refused (401/403): nothing retained is
    /// accessible any more.
    pub fn deny_all(&mut self) {
        self.touch();
        self.access_revision += 1;
        self.complete = true;
        let ids: Vec<String> = self
            .rosters
            .keys()
            .chain(self.metadata.keys())
            .cloned()
            .collect();
        self.denied.extend(ids);
    }

    /// A restored snapshot grants local display only for known signed
    /// channels; close the unknown-access boundary before hydrating content.
    pub fn restrict_to_known(&mut self) {
        if self.complete {
            return;
        }
        self.complete = true;
        self.touch();
        self.access_revision += 1;
    }

    /// May the viewer read this channel? Unknown channels stay readable
    /// until a complete roster or a local snapshot closes the boundary.
    pub fn can_access(&self, id: &str) -> bool {
        if self.denied.contains(id) || self.suspended.contains(id) {
            return false;
        }
        let roster = self.rosters.get(id);
        if roster.is_some_and(|roster| has_tag(roster, "p", &self.viewer)) {
            return true;
        }
        self.open(id) || (roster.is_none() && !self.metadata.contains_key(id) && !self.complete)
    }

    /// Explicit signed public metadata grants reading, never membership.
    fn open(&self, id: &str) -> bool {
        self.metadata
            .get(id)
            .is_some_and(|event| open_metadata(event))
    }

    /// May the viewer write here? Not while membership is only cached.
    pub fn can_participate(&self, id: &str) -> bool {
        if !self.can_access(id) || self.cached.contains(id) {
            return false;
        }
        match self.rosters.get(id) {
            Some(roster) => has_tag(roster, "p", &self.viewer),
            None => !self.complete && !self.open(id),
        }
    }

    /// The viewer is a member per the newest signed roster.
    pub fn authorized(&self, id: &str) -> bool {
        !self.denied.contains(id)
            && self
                .rosters
                .get(id)
                .is_some_and(|roster| has_tag(roster, "p", &self.viewer))
    }

    pub fn metadata_version(&self, id: &str) -> Option<&Verified> {
        self.metadata.get(id)
    }

    /// Metadata is retained for this channel.
    pub fn named(&self, id: &str) -> bool {
        self.metadata.contains_key(id)
    }

    /// The retained metadata came from disk and should be re-read.
    pub fn pending_metadata(&self, id: &str) -> bool {
        self.pending_metadata.contains(id)
    }

    pub fn name(&self, id: &str) -> String {
        self.metadata
            .get(id)
            .and_then(|event| metadata_name(event))
            .unwrap_or_else(|| id.chars().take(8).collect())
    }

    pub fn hidden(&self, id: &str) -> bool {
        self.metadata
            .get(id)
            .is_some_and(|event| has_tag_named(event, "hidden"))
    }

    pub fn is_private(&self, id: &str) -> bool {
        self.metadata
            .get(id)
            .is_some_and(|event| has_tag_named(event, "private"))
    }

    /// A private stream whose `about` is session metadata.
    pub fn is_session(&self, id: &str) -> bool {
        self.metadata.get(id).is_some_and(|event| {
            tag(event, "t") == Some("stream")
                && has_tag_named(event, "private")
                && session_metadata(tag(event, "about")).is_some()
        })
    }

    /// `Some(read_only)` when [`Self::get`] would return a summary.
    fn preview(&self, id: &str) -> Option<bool> {
        if !self.can_access(id) {
            return None;
        }
        let authorized = self.authorized(id);
        if !authorized && !self.open(id) {
            return None;
        }
        Some(!authorized || self.cached.contains(id))
    }

    /// The channel as the UI sees it, or `None` when it is neither joined
    /// nor an open public preview (`discovery.ts:200-270`).
    pub fn get(&self, id: &str) -> Option<ChannelSummary> {
        let read_only = self.preview(id)?;
        let event = self.metadata.get(id);
        let roster = self.rosters.get(id);
        let channel_type = if self.is_session(id) {
            Some(ChannelType::Session)
        } else {
            match event.and_then(|event| tag(event, "t")) {
                Some("stream") => Some(ChannelType::Stream),
                Some("forum") => Some(ChannelType::Forum),
                Some("dm") => Some(ChannelType::Dm),
                _ => None,
            }
        };
        let is_session = channel_type == Some(ChannelType::Session);
        let parent = event
            .and_then(|event| session_metadata(tag(event, "about")))
            .flatten();
        let identity = event
            .map(|event| {
                crate::channel_labels::identity(event.tags.iter().map(|tag| tag.as_slice()))
            })
            .unwrap_or_default();
        Some(ChannelSummary {
            id: id.to_owned(),
            name: self.name(id),
            description: event
                .filter(|_| !is_session)
                .map(|event| tag(event, "about").unwrap_or_default().to_owned()),
            visibility: event.and_then(|event| channel_visibility(event)),
            last_activity_at: None,
            read_only,
            cached: self.cached.contains(id),
            hidden: self.hidden(id),
            private: self.is_private(id),
            archived: event.is_some_and(|event| has_tag(event, "archived", "true")),
            channel_type,
            parent_channel_id: if is_session { parent } else { None },
            updated_at: event
                .filter(|_| is_session)
                .map(|event| event.created_at.as_secs()),
            members: sorted_unique(roster_members(roster)),
            participants: (channel_type == Some(ChannelType::Dm)).then(|| {
                sorted_unique(roster_members(roster).filter(|member| *member != self.viewer))
            }),
            preview: None,
            labels: identity.labels,
            creator: identity.creator,
            created_at: identity.created_at,
        })
    }

    /// Forgets membership restored from disk that the network has not
    /// confirmed. A fresh identical signed roster can grant it again.
    pub fn clear_cached(&mut self) {
        let mut cached: Vec<String> = self.cached.drain().collect();
        if cached.is_empty() {
            return;
        }
        self.touch();
        cached.sort();
        for id in cached {
            self.deny(&id);
            self.rosters.remove(&id);
            self.metadata.remove(&id);
            self.pending_metadata.remove(&id);
        }
    }

    /// Network-confirmed rosters and their metadata, for the startup cache.
    pub fn saved_events(&self) -> Vec<Verified> {
        let mut events = Vec::new();
        for (id, roster) in &self.rosters {
            if !self.authorized(id) || self.cached.contains(id) {
                continue;
            }
            events.push(roster.clone());
            if let Some(metadata) = self.metadata.get(id) {
                events.push(metadata.clone());
            }
        }
        events
    }

    /// The sidebar roster: joined channels only, by name then id.
    pub fn channels(&self) -> Vec<ChannelSummary> {
        let mut channels: Vec<ChannelSummary> = self
            .rosters
            .keys()
            .filter(|id| self.authorized(id))
            .filter_map(|id| self.get(id))
            .collect();
        channels.sort_by(|a, b| {
            locale_compare(&a.name, &b.name).then_with(|| locale_compare(&a.id, &b.id))
        });
        channels
    }

    /// Ids an exact resolution should read: those not joined, or joined
    /// only from cache (`store.ts` `resolve`).
    pub fn resolve_targets(&self, ids: &[String]) -> Result<Vec<String>, DiscoveryError> {
        let mut seen = HashSet::new();
        let targets: Vec<String> = ids
            .iter()
            .filter(|id| seen.insert(id.as_str()))
            .filter(|id| !self.authorized(id) || self.cached.contains(id.as_str()))
            .cloned()
            .collect();
        if targets.len() > RESOLVE_LIMIT {
            return Err(DiscoveryError::TooManyChannels);
        }
        Ok(targets)
    }

    /// Captures what an exact resolution of `ids` must compare against.
    pub fn resolution_start(&self, ids: &[String]) -> ResolutionStart {
        ResolutionStart {
            metadata: ids
                .iter()
                .map(|id| (id.clone(), self.metadata.get(id).map(|event| event.id)))
                .collect(),
            rosters: self.roster_versions(),
        }
    }

    /// Applies the answer to [`resolve_filters`] for `ids`.
    ///
    /// An exact viewer-scoped read is complete for its ids, unlike a capped
    /// roster scan: a cached membership it omits is lost, and a read-only
    /// or suspended preview whose metadata is missing is no longer
    /// readable. Those channels are denied here and returned so the
    /// channel store can purge them (§8.4).
    pub fn apply_resolution(
        &mut self,
        ids: &[String],
        start: &ResolutionStart,
        events: &[Verified],
    ) -> Result<Resolution, DiscoveryError> {
        let count = |kind| events.iter().filter(|e| e.kind_u16() == kind).count();
        if count(kinds::CHANNEL_METADATA) > ids.len() || count(kinds::CHANNEL_MEMBERS) > ids.len() {
            return Err(DiscoveryError::InvalidResponse(
                "Channel discovery exceeded its read budget",
            ));
        }
        let relay = self.relay_author.clone();
        let viewer = self.viewer.clone();
        let from_relay = |event: &Verified, id: &str| {
            event.pubkey_hex() == relay && channel_id(event) == Some(id)
        };
        let member_roster = |event: &Verified, id: &str| {
            event.kind_u16() == kinds::CHANNEL_MEMBERS
                && from_relay(event, id)
                && has_tag(event, "p", &viewer)
        };
        let mut resolution = Resolution::default();
        for id in ids {
            if events.iter().any(|event| {
                (event.kind_u16() == kinds::CHANNEL_METADATA && from_relay(event, id))
                    || member_roster(event, id)
            }) {
                resolution.resumed |= self.resume(id);
            }
        }
        let ordered: Vec<Verified> = events
            .iter()
            .filter(|event| event.kind_u16() == kinds::CHANNEL_MEMBERS)
            .chain(
                events
                    .iter()
                    .filter(|event| event.kind_u16() != kinds::CHANNEL_MEMBERS),
            )
            .cloned()
            .collect();
        resolution.applied = self.apply(&ordered, None, None, false);
        for id in ids {
            let cached = self.preview(id).is_some() && self.cached.contains(id);
            if cached
                && self.rosters.get(id).map(|roster| roster.id) == start.rosters.get(id).copied()
                && !events.iter().any(|event| member_roster(event, id))
            {
                self.deny(id);
                resolution.denied.push(id.clone());
            }
            let has_metadata = events
                .iter()
                .any(|event| event.kind_u16() == kinds::CHANNEL_METADATA && from_relay(event, id));
            if !has_metadata {
                if (self.preview(id) == Some(true) || self.suspended.contains(id))
                    && start.metadata.get(id).copied().flatten()
                        == self.metadata.get(id).map(|event| event.id)
                {
                    self.deny(id);
                    resolution.denied.push(id.clone());
                }
            } else if !self.named(id) {
                resolution.metadata_capacity_exhausted = true;
                break;
            }
        }
        Ok(resolution)
    }
}

/// Versions captured before an exact resolution read.
#[derive(Clone, Debug, Default)]
pub struct ResolutionStart {
    metadata: HashMap<String, Option<EventId>>,
    rosters: RosterVersions,
}

/// Result of [`DiscoveryState::apply_resolution`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    pub applied: Applied,
    /// A suspended preview was resumed by fresh evidence.
    pub resumed: bool,
    /// Channels denied by this resolution, message "Conversation is
    /// unavailable". The store purges their data.
    pub denied: Vec<String>,
    /// Signed metadata arrived but retention is full ("Channel metadata
    /// capacity unavailable"). Processing stopped at that channel.
    pub metadata_capacity_exhausted: bool,
}

fn strong(filter: &mut Filter, strong: bool) {
    if strong {
        filter.insert("consistency".into(), json!("strong"));
    }
}

/// One page of the viewer-scoped roster scan (§5a.2 step 1). `cursor` is
/// the last kept roster of the previous page.
pub fn roster_page_filter(
    viewer: &str,
    consistent: bool,
    cursor: Option<(u64, &EventId)>,
) -> Filter {
    let mut f = filter(json!({
        "kinds": [kinds::CHANNEL_MEMBERS],
        "#p": [viewer],
        "limit": DISCOVERY_LIMIT,
    }));
    strong(&mut f, consistent);
    if let Some((created_at, id)) = cursor {
        f.insert("until".into(), json!(created_at));
        f.insert("before_id".into(), json!(id.to_hex()));
    }
    f
}

/// Exact re-read of memberships a multi-page scan omitted (§5a.2 step 2).
pub fn confirmation_filter(
    relay_author: &str,
    viewer: &str,
    batch: &[String],
    consistent: bool,
) -> Filter {
    let mut f = filter(json!({
        "kinds": [kinds::CHANNEL_MEMBERS],
        "authors": [relay_author],
        "#d": batch,
        "#p": [viewer],
        "limit": batch.len() + 1,
    }));
    strong(&mut f, consistent);
    f
}

/// Metadata for joined channels that lack it (§5a.2 step 3). No `authors`
/// filter; the author is checked on accept.
pub fn metadata_filter(ids: &[String], consistent: bool) -> Filter {
    let mut f = filter(json!({
        "kinds": [kinds::CHANNEL_METADATA],
        "#d": ids,
        "limit": DISCOVERY_LIMIT,
    }));
    strong(&mut f, consistent);
    f
}

/// Exact metadata and viewer roster for up to [`RESOLVE_LIMIT`] channels
/// (`store.ts` `resolve`). The reader must not coalesce it with a read in
/// flight (`fresh`).
pub fn resolve_filters(
    relay_author: &str,
    viewer: &str,
    ids: &[String],
    consistent: bool,
) -> Vec<Filter> {
    let mut metadata = filter(json!({
        "kinds": [kinds::CHANNEL_METADATA],
        "authors": [relay_author],
        "#d": ids,
        "limit": ids.len() + 1,
    }));
    let mut roster = filter(json!({
        "kinds": [kinds::CHANNEL_MEMBERS],
        "authors": [relay_author],
        "#d": ids,
        "#p": [viewer],
        "limit": ids.len() + 1,
    }));
    strong(&mut metadata, consistent);
    strong(&mut roster, consistent);
    vec![metadata, roster]
}

/// Live subscription for membership notifications (§5a.2 step 5).
pub fn membership_hint_filter(viewer: &str) -> Filter {
    filter(json!({
        "kinds": [kinds::MEMBER_ADDED, kinds::MEMBER_REMOVED],
        "#p": [viewer],
    }))
}

/// What a membership notification asks the session to re-read. These are
/// hints only, never roster authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MembershipHint {
    /// A 44100 for exactly one channel: resolve that channel exactly.
    Resolve(String),
    /// Anything else: a full strong roster refresh.
    Refresh,
}

/// Parses a 44100/44101 notification. Only relay-signed notifications
/// addressed to the viewer count (`session.ts:2117-2140,2340-2356`). The
/// session still falls back to [`MembershipHint::Refresh`] when the list
/// is not ready, a full pass is already scheduled, or the channel is held.
pub fn membership_hint(
    event: &Verified,
    viewer: &str,
    relay_author: &str,
) -> Option<MembershipHint> {
    let kind = event.kind_u16();
    if (kind != kinds::MEMBER_ADDED && kind != kinds::MEMBER_REMOVED)
        || event.pubkey_hex() != relay_author
        || !event.has_tag("p", viewer)
    {
        return None;
    }
    let destinations: Vec<&[String]> = event.tags_named("h").collect();
    match destinations.as_slice() {
        [only] if kind == kinds::MEMBER_ADDED => match only.get(1) {
            Some(id) if !id.is_empty() => Some(MembershipHint::Resolve(id.clone())),
            _ => Some(MembershipHint::Refresh),
        },
        _ => Some(MembershipHint::Refresh),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MembershipChangeType {
    Joined,
    Left,
    Removed,
}

/// Relay-authored membership activity (kind 40099): a timeline row, not a
/// membership grant (`membership.ts:12-35`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipChange {
    pub kind: MembershipChangeType,
    pub actor: String,
    pub target: String,
}

/// Only the relay's own key may describe membership activity.
pub fn membership_change(event: &Event, relay_author: &str) -> Option<MembershipChange> {
    if event.kind.as_u16() != kinds::SYSTEM_MESSAGE || event.pubkey.to_hex() != relay_author {
        return None;
    }
    let Ok(Value::Object(body)) = serde_json::from_str::<Value>(&event.content) else {
        return None;
    };
    let pubkey = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .filter(|v| is_hex64(v))
    };
    let actor = pubkey("actor")?;
    match body.get("type").and_then(Value::as_str) {
        Some("member_left") => {
            let target = body.get("target");
            if target.is_some() && target.and_then(Value::as_str) != Some(actor) {
                return None;
            }
            Some(MembershipChange {
                kind: MembershipChangeType::Left,
                actor: actor.to_owned(),
                target: actor.to_owned(),
            })
        }
        Some(kind @ ("member_joined" | "member_removed")) => Some(MembershipChange {
            kind: if kind == "member_joined" {
                MembershipChangeType::Joined
            } else {
                MembershipChangeType::Removed
            },
            actor: actor.to_owned(),
            target: pubkey("target")?.to_owned(),
        }),
        _ => None,
    }
}

/// Where a [`RosterScan`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanStage {
    /// Reading viewer-scoped roster pages. A 401/403 here means the viewer
    /// lost the whole community.
    Roster,
    /// Re-reading memberships the pages omitted.
    Confirm,
    /// Reading names for joined channels.
    Metadata,
    Done,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// Ask for `consistency: "strong"` (a writer is owed a strong re-list).
    pub strong: bool,
    /// Re-read metadata even for channels that already have it.
    pub force_metadata: bool,
}

/// The next read a scan needs.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanRead {
    pub filter: Filter,
    /// Must not join an identical read already in flight.
    pub fresh: bool,
}

/// One full roster pass (`store.ts:1015-1210`).
///
/// Call [`Self::next_read`], run it through the reader, and hand the
/// verified events to [`Self::accept`] until the stage is
/// [`ScanStage::Done`]. On a read error call [`Self::fail`]. Dropping a scan
/// (revocation, disposal) is always safe: state only changes in `accept`.
#[derive(Debug)]
pub struct RosterScan {
    viewer: String,
    relay_author: String,
    options: ScanOptions,
    max_page: usize,
    started: RosterVersions,
    overflow_revision: u64,
    ids: Vec<String>,
    id_set: HashSet<String>,
    named: HashSet<String>,
    cursor: Option<(u64, EventId)>,
    page: usize,
    paged: bool,
    complete: Option<HashSet<String>>,
    omitted: Vec<String>,
    wanted: Vec<String>,
    offset: usize,
    stage: ScanStage,
    certified: bool,
}

impl RosterScan {
    pub fn new(state: &DiscoveryState, options: ScanOptions) -> Self {
        Self {
            viewer: state.viewer.clone(),
            relay_author: state.relay_author.clone(),
            options,
            // The retained budget plus its final exhaustion read; this also
            // bounds a relay repeating coordinates with new versions.
            max_page: state.capacity / DISCOVERY_LIMIT,
            started: state.roster_versions(),
            overflow_revision: state.overflow_revision,
            ids: Vec::new(),
            id_set: HashSet::new(),
            named: HashSet::new(),
            cursor: None,
            page: 0,
            paged: false,
            complete: None,
            omitted: Vec::new(),
            wanted: Vec::new(),
            offset: 0,
            stage: ScanStage::Roster,
            certified: false,
        }
    }

    pub fn stage(&self) -> ScanStage {
        self.stage
    }

    /// The scan proved the roster complete: coverage is no longer partial.
    pub fn certified(&self) -> bool {
        self.certified
    }

    /// Channel ids the roster pages returned, in page order.
    pub fn ids(&self) -> &[String] {
        &self.ids
    }

    pub fn next_read(&self) -> Option<ScanRead> {
        let strong = self.options.strong;
        match self.stage {
            ScanStage::Roster => Some(ScanRead {
                filter: roster_page_filter(
                    &self.viewer,
                    strong,
                    self.cursor.as_ref().map(|(at, id)| (*at, id)),
                ),
                fresh: false,
            }),
            ScanStage::Confirm => Some(ScanRead {
                filter: confirmation_filter(
                    &self.relay_author,
                    &self.viewer,
                    self.batch(DISCOVERY_CONFIRM_LIMIT, &self.omitted),
                    strong,
                ),
                fresh: true,
            }),
            ScanStage::Metadata => Some(ScanRead {
                filter: metadata_filter(self.batch(DISCOVERY_LIMIT, &self.wanted), strong),
                fresh: false,
            }),
            ScanStage::Done => None,
        }
    }

    fn batch<'a>(&self, size: usize, ids: &'a [String]) -> &'a [String] {
        &ids[self.offset..(self.offset + size).min(ids.len())]
    }

    /// Feeds the verified answer to [`Self::next_read`].
    pub fn accept(
        &mut self,
        state: &mut DiscoveryState,
        events: &[Verified],
    ) -> Result<(), DiscoveryError> {
        let result = match self.stage {
            ScanStage::Roster => self.accept_page(state, events),
            ScanStage::Confirm => self.accept_confirmation(state, events),
            ScanStage::Metadata => {
                state.apply(events, None, None, false);
                self.offset += DISCOVERY_LIMIT;
                if self.offset >= self.wanted.len() {
                    self.stage = ScanStage::Done;
                }
                Ok(())
            }
            ScanStage::Done => Ok(()),
        };
        if result.is_err() {
            self.stage = ScanStage::Done;
        }
        result
    }

    /// The current read failed. A denial while reading roster pages denies
    /// everything (§8.4); later failures keep the membership already
    /// established. Returns whether everything was denied.
    pub fn fail(&mut self, state: &mut DiscoveryState, denied: bool) -> bool {
        let deny_all = denied && self.stage == ScanStage::Roster;
        if deny_all {
            state.deny_all();
        }
        self.stage = ScanStage::Done;
        deny_all
    }

    fn accept_page(
        &mut self,
        state: &mut DiscoveryState,
        events: &[Verified],
    ) -> Result<(), DiscoveryError> {
        let mut members: Vec<&Verified> = events
            .iter()
            .filter(|event| {
                event.kind_u16() == kinds::CHANNEL_MEMBERS
                    && event.pubkey_hex() == self.relay_author
                    && has_tag(event, "p", &self.viewer)
                    && channel_id(event).is_some()
            })
            .collect();
        members.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        for event in &members {
            if let Some(id) = channel_id(event)
                && self.id_set.insert(id.to_owned())
            {
                self.ids.push(id.to_owned());
            }
        }
        for event in events {
            if event.kind_u16() == kinds::CHANNEL_METADATA
                && event.pubkey_hex() == self.relay_author
                && let Some(id) = channel_id(event)
            {
                self.named.insert(id.to_owned());
            }
        }
        let last = members
            .last()
            .map(|event| (event.created_at.as_secs(), event.id));
        let advances = members.iter().all(|event| match &self.cursor {
            None => true,
            Some((at, id)) => {
                let created_at = event.created_at.as_secs();
                created_at < *at || (created_at == *at && event.id > *id)
            }
        }) && (events.len() < DISCOVERY_LIMIT || last.is_some());
        let short = advances && events.len() < DISCOVERY_LIMIT;
        let page_complete = (short && !self.paged).then(|| self.id_set.clone());
        let applied = state.apply(events, page_complete.as_ref(), Some(&self.started), false);
        let overflowed = state.overflow_revision != self.overflow_revision;
        if !advances {
            return Err(DiscoveryError::InvalidResponse(
                "Channel discovery cursor did not advance",
            ));
        }
        if short && !self.paged {
            // A single short page already retained itself.
            self.certified = applied.certified;
        }
        if short && !overflowed {
            if self.paged {
                self.complete = Some(self.id_set.clone());
            }
            self.after_pages(state);
            return Ok(());
        }
        if overflowed {
            self.after_pages(state);
            return Ok(());
        }
        self.cursor = last;
        self.paged = true;
        self.page += 1;
        if self.page > self.max_page {
            self.after_pages(state);
        }
        Ok(())
    }

    fn after_pages(&mut self, state: &mut DiscoveryState) {
        let Some(complete) = &self.complete else {
            self.start_metadata(state);
            return;
        };
        let mut omitted: Vec<String> = self
            .started
            .iter()
            .filter(|(id, version)| {
                !complete.contains(*id)
                    && state.roster(id).is_some_and(|roster| {
                        roster.id == **version && has_tag(roster, "p", &self.viewer)
                    })
                    && state.authorized(id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        omitted.sort();
        if omitted.is_empty() {
            self.finish_confirmation(state);
        } else {
            self.omitted = omitted;
            self.offset = 0;
            self.stage = ScanStage::Confirm;
        }
    }

    fn accept_confirmation(
        &mut self,
        state: &mut DiscoveryState,
        events: &[Verified],
    ) -> Result<(), DiscoveryError> {
        let batch = self.batch(DISCOVERY_CONFIRM_LIMIT, &self.omitted);
        if events.len() > batch.len()
            || events.iter().any(|event| {
                event.kind_u16() != kinds::CHANNEL_MEMBERS
                    || event.pubkey_hex() != self.relay_author
                    || !has_tag(event, "p", &self.viewer)
                    || !batch.iter().any(|id| Some(id.as_str()) == tag(event, "d"))
            })
        {
            return Err(DiscoveryError::InvalidResponse(
                "Channel discovery confirmation exceeded its read budget",
            ));
        }
        if let Some(complete) = &mut self.complete {
            complete.extend(
                events
                    .iter()
                    .filter_map(|e| channel_id(e))
                    .map(str::to_owned),
            );
        }
        state.apply(events, None, None, false);
        self.offset += DISCOVERY_CONFIRM_LIMIT;
        if self.offset >= self.omitted.len() {
            self.finish_confirmation(state);
        }
        Ok(())
    }

    fn finish_confirmation(&mut self, state: &mut DiscoveryState) {
        if state.overflow_revision != self.overflow_revision {
            self.complete = None;
        }
        if let Some(complete) = self.complete.take() {
            self.certified = state
                .apply(&[], Some(&complete), Some(&self.started), false)
                .certified;
        }
        self.start_metadata(state);
    }

    fn start_metadata(&mut self, state: &DiscoveryState) {
        self.wanted = self
            .ids
            .iter()
            .filter(|id| {
                state.authorized(id)
                    && !self.named.contains(*id)
                    && (self.options.force_metadata
                        || state.pending_metadata(id)
                        || !state.named(id))
            })
            .cloned()
            .collect();
        self.offset = 0;
        self.stage = if self.wanted.is_empty() {
            ScanStage::Done
        } else {
            ScanStage::Metadata
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Verifier;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};

    const T0: u64 = 1_700_000_000;

    struct Fixture {
        relay: Keys,
        viewer: Keys,
        verifier: Verifier,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                relay: Keys::generate(),
                viewer: Keys::generate(),
                verifier: Verifier::new(4096),
            }
        }

        fn viewer_hex(&self) -> String {
            self.viewer.public_key().to_hex()
        }

        fn relay_hex(&self) -> String {
            self.relay.public_key().to_hex()
        }

        fn state(&self) -> DiscoveryState {
            DiscoveryState::new(self.viewer_hex(), self.relay_hex())
        }

        fn signed(
            &self,
            keys: &Keys,
            kind: u16,
            at: u64,
            content: &str,
            tags: Vec<Vec<String>>,
        ) -> Verified {
            let tags = tags.into_iter().map(|tag| Tag::parse(tag).unwrap());
            let event = EventBuilder::new(Kind::from_u16(kind), content)
                .tags(tags)
                .custom_created_at(Timestamp::from_secs(at))
                .finalize(keys)
                .unwrap();
            self.verifier.verify(event).unwrap()
        }

        fn roster_by(&self, keys: &Keys, id: &str, members: &[String], at: u64) -> Verified {
            let mut tags = vec![vec!["d".to_owned(), id.to_owned()]];
            tags.extend(members.iter().map(|m| vec!["p".to_owned(), m.clone()]));
            self.signed(keys, kinds::CHANNEL_MEMBERS, at, "", tags)
        }

        fn roster(&self, id: &str, members: &[String], at: u64) -> Verified {
            self.roster_by(&self.relay, id, members, at)
        }

        fn member(&self, id: &str) -> Verified {
            self.roster(id, &[self.viewer_hex()], T0)
        }

        fn metadata(&self, id: &str, at: u64, tags: &[&[&str]]) -> Verified {
            let mut all = vec![vec!["d".to_owned(), id.to_owned()]];
            all.extend(
                tags.iter()
                    .map(|t| t.iter().map(|s| s.to_string()).collect()),
            );
            self.signed(&self.relay, kinds::CHANNEL_METADATA, at, "", all)
        }

        fn named(&self, id: &str, name: &str) -> Verified {
            self.metadata(id, T0, &[&["name", name]])
        }
    }

    fn uuid(i: usize) -> String {
        format!("00000000-0000-4000-8000-{i:012}")
    }

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// `count` same-timestamp memberships sorted the way the relay pages
    /// them (created_at desc, id asc).
    fn memberships(f: &Fixture, count: usize) -> Vec<Verified> {
        let mut events: Vec<Verified> = (0..count).map(|i| f.member(&uuid(i))).collect();
        events.sort_by_key(|e| e.id);
        events
    }

    #[test]
    fn accepts_only_relay_signed_channel_state() {
        let f = Fixture::new();
        let mut state = f.state();
        let viewer = f.viewer_hex();
        let forged = f.roster_by(&f.viewer, "general", std::slice::from_ref(&viewer), T0);
        assert!(!state.accept(&forged, false));
        assert!(!state.authorized("general"));
        let wrong_kind = f.signed(
            &f.relay,
            kinds::CHANNEL_ADMINS,
            T0,
            "",
            vec![vec!["d".into(), "general".into()]],
        );
        assert!(!state.accept(&wrong_kind, false));
        let no_d = f.signed(
            &f.relay,
            kinds::CHANNEL_MEMBERS,
            T0,
            "",
            vec![vec!["p".into(), viewer.clone()]],
        );
        assert!(!state.accept(&no_d, false));
        assert!(state.accept(&f.member("general"), false));
        assert!(state.authorized("general"));
        assert!(state.can_participate("general"));
    }

    #[test]
    fn keeps_the_newest_version_and_denies_on_removal() {
        let f = Fixture::new();
        let mut state = f.state();
        let viewer = f.viewer_hex();
        state.accept(&f.roster("c", std::slice::from_ref(&viewer), 10), false);
        // An older replay cannot remove the viewer.
        assert!(!state.accept(&f.roster("c", &[], 9), false));
        assert!(state.authorized("c"));
        let revision = state.access_revision();
        assert!(state.accept(&f.roster("c", &[], 11), false));
        assert!(!state.authorized("c"));
        assert!(!state.can_access("c"));
        assert!(state.access_revision() > revision);
        // A newer roster that lists the viewer again regrants access.
        assert!(state.accept(&f.roster("c", std::slice::from_ref(&viewer), 12), false));
        assert!(state.authorized("c"));
    }

    #[test]
    fn unknown_channels_are_readable_until_the_boundary_closes() {
        let f = Fixture::new();
        let mut state = f.state();
        assert!(state.can_access("unknown"));
        assert!(state.can_participate("unknown"));
        assert!(!state.authorized("unknown"));
        assert!(state.get("unknown").is_none());
        state.retain(&HashSet::new(), &RosterVersions::new());
        assert!(!state.can_access("unknown"));
        let mut restored = f.state();
        restored.restrict_to_known();
        assert!(!restored.can_access("unknown"));
    }

    #[test]
    fn open_metadata_grants_a_read_only_preview_never_membership() {
        let f = Fixture::new();
        let mut state = f.state();
        state.retain(&HashSet::new(), &RosterVersions::new());
        state.accept(
            &f.metadata(
                "pub",
                T0,
                &[&["name", "Lobby"], &["public"], &["t", "stream"]],
            ),
            false,
        );
        assert!(state.can_access("pub"));
        assert!(!state.can_participate("pub"));
        assert!(!state.authorized("pub"));
        let preview = state.get("pub").unwrap();
        assert!(preview.read_only);
        assert_eq!(preview.visibility, Some(Visibility::Public));
        assert!(state.channels().is_empty());

        let cases: [&[&[&str]]; 3] = [
            &[&["public"], &["private"]],
            &[&["public"], &["hidden"]],
            &[&["public"], &["t", "dm"]],
        ];
        for tags in cases {
            let mut state = f.state();
            state.retain(&HashSet::new(), &RosterVersions::new());
            state.accept(&f.metadata("x", T0, tags), false);
            assert!(!state.can_access("x"), "{tags:?}");
        }
    }

    #[test]
    fn cached_membership_is_display_only_until_confirmed() {
        let f = Fixture::new();
        let mut state = f.state();
        let viewer = f.viewer_hex();
        let disk = f.roster("c", std::slice::from_ref(&viewer), 10);
        state.apply(std::slice::from_ref(&disk), None, None, true);
        assert!(state.is_complete());
        let summary = state.get("c").unwrap();
        assert!(summary.cached && summary.read_only);
        assert!(!state.can_participate("c"));
        // An older network version must not confirm the newer disk copy.
        assert!(!state.accept(&f.roster("c", std::slice::from_ref(&viewer), 9), false));
        assert!(state.get("c").unwrap().cached);
        // The same version from the network confirms it.
        assert!(state.accept(&disk, false));
        let summary = state.get("c").unwrap();
        assert!(!summary.cached && !summary.read_only);
        assert!(state.can_participate("c"));
    }

    #[test]
    fn clearing_cached_membership_denies_until_fresh_evidence() {
        let f = Fixture::new();
        let mut state = f.state();
        let disk = f.member("c");
        state.apply(std::slice::from_ref(&disk), None, None, true);
        state.clear_cached();
        assert!(!state.can_access("c"));
        assert!(state.roster("c").is_none());
        state.accept(&disk, false);
        assert!(state.authorized("c"));
    }

    #[test]
    fn retention_overflow_is_counted_not_stored() {
        let f = Fixture::new();
        let mut state = DiscoveryState::with_capacity(f.viewer_hex(), f.relay_hex(), 2);
        let applied = state.apply(
            &[f.member("a"), f.member("b"), f.member("c")],
            None,
            None,
            false,
        );
        assert!(applied.overflowed);
        assert_eq!(state.overflow_revision(), 1);
        assert!(state.roster("c").is_none());
        // Overflow voids completeness, so nothing is revoked by omission.
        let applied = state.apply(&[f.member("d")], Some(&set(&["d"])), None, false);
        assert!(!applied.certified);
        assert!(state.authorized("a"));
    }

    #[test]
    fn denial_never_evicts_and_falls_back_to_deny_all() {
        let f = Fixture::new();
        let mut state = DiscoveryState::with_capacity(f.viewer_hex(), f.relay_hex(), 2);
        state.apply(&[f.member("a"), f.member("b")], None, None, false);
        state.deny("x");
        state.deny("y");
        assert!(state.can_access("a"));
        // The denied set is full: fail closed for everything.
        state.deny("z");
        assert!(!state.can_access("a"));
        assert!(!state.can_access("b"));
        assert!(!state.can_access("unknown"));
    }

    #[test]
    fn complete_scan_omission_denies_only_unchanged_rosters() {
        let f = Fixture::new();
        let mut state = f.state();
        let viewer = f.viewer_hex();
        state.apply(
            &[
                f.roster("kept", std::slice::from_ref(&viewer), 10),
                f.roster("omitted", std::slice::from_ref(&viewer), 10),
                f.roster("renewed", std::slice::from_ref(&viewer), 10),
            ],
            None,
            None,
            false,
        );
        let started = state.roster_versions();
        // A live grant lands between the scan start and its completion.
        state.accept(
            &f.roster("renewed", std::slice::from_ref(&viewer), 11),
            false,
        );
        let applied = state.apply(&[], Some(&set(&["kept"])), Some(&started), false);
        assert!(applied.certified && applied.access_changed);
        assert!(state.authorized("kept"));
        assert!(!state.authorized("omitted"));
        assert!(state.authorized("renewed"));
    }

    /// Every mutation that changes what the session derives from the state
    /// moves the revision, and a no-op leaves it alone.
    #[test]
    fn revision_follows_every_visible_change() {
        let f = Fixture::new();
        let viewer = f.viewer_hex();
        let ids = ["a", "b", "pub", "unknown"];
        let observe = |state: &DiscoveryState| {
            let per_id: Vec<_> = ids
                .iter()
                .map(|id| {
                    (
                        state.get(id),
                        state.can_access(id),
                        state.can_participate(id),
                        state.authorized(id),
                    )
                })
                .collect();
            format!("{:?}{per_id:?}", state.channels())
        };
        let mut state = f.state();
        let disk = f.member("a");
        type Step<'a> = (&'a str, bool, Box<dyn Fn(&mut DiscoveryState) + 'a>);
        let steps: Vec<Step> = vec![
            (
                "restore",
                true,
                Box::new(|s| {
                    s.apply(std::slice::from_ref(&disk), None, None, true);
                }),
            ),
            (
                "confirm",
                true,
                Box::new(|s| {
                    s.accept(&disk, false);
                }),
            ),
            (
                "join",
                true,
                Box::new(|s| {
                    s.accept(&f.member("b"), false);
                }),
            ),
            (
                "rename",
                true,
                Box::new(|s| {
                    s.accept(&f.named("b", "Beta"), false);
                }),
            ),
            (
                "stale rename",
                false,
                Box::new(|s| {
                    s.accept(&f.metadata("b", T0 - 1, &[&["name", "Old"]]), false);
                }),
            ),
            (
                "roster",
                true,
                Box::new(|s| {
                    s.accept(
                        &f.roster("b", &[viewer.clone(), f.relay_hex()], T0 + 1),
                        false,
                    );
                }),
            ),
            (
                "preview",
                true,
                Box::new(|s| {
                    s.accept(&f.metadata("pub", T0, &[&["public"]]), false);
                }),
            ),
            (
                "suspend",
                true,
                Box::new(|s| {
                    s.suspend("pub");
                }),
            ),
            (
                "resume",
                true,
                Box::new(|s| {
                    s.resume("pub");
                }),
            ),
            ("deny", true, Box::new(|s| s.deny("a"))),
            ("deny again", false, Box::new(|s| s.deny("a"))),
            (
                "leave",
                true,
                Box::new(|s| {
                    s.accept(&f.roster("b", &[], T0 + 2), false);
                }),
            ),
            (
                "regrant",
                true,
                Box::new(|s| {
                    s.accept(&f.roster("b", std::slice::from_ref(&viewer), T0 + 3), false);
                }),
            ),
            ("deny all", true, Box::new(|s| s.deny_all())),
        ];
        for (name, changes, step) in &steps {
            let (before, revision) = (observe(&state), state.revision());
            step(&mut state);
            assert_eq!(state.revision() != revision, *changes, "{name}");
            assert!(observe(&state) == before || *changes, "{name}");
        }

        let mut state = f.state();
        let revision = state.revision();
        state.retain(&HashSet::new(), &RosterVersions::new());
        assert_ne!(state.revision(), revision, "retain");
        let revision = state.revision();
        state.restrict_to_known();
        assert_eq!(state.revision(), revision, "restrict when complete");
        let mut state = f.state();
        let revision = state.revision();
        state.restrict_to_known();
        assert_ne!(state.revision(), revision, "restrict");
        let mut state = f.state();
        state.apply(&[f.member("c")], None, None, true);
        let revision = state.revision();
        state.clear_cached();
        assert_ne!(state.revision(), revision, "clear cached");
        let revision = state.revision();
        state.clear_cached();
        assert_eq!(state.revision(), revision, "clear nothing");
        // A replacement state never repeats a revision.
        assert_ne!(f.state().revision(), f.state().revision());
    }

    #[test]
    fn suspends_previews_only() {
        let f = Fixture::new();
        let mut state = f.state();
        state.accept(&f.member("joined"), false);
        state.accept(&f.metadata("pub", T0, &[&["public"]]), false);
        assert!(!state.suspend("joined"));
        assert!(state.suspend("pub"));
        assert!(!state.can_access("pub"));
        assert_eq!(state.suspended_channels(), vec!["pub".to_owned()]);
        assert!(state.resume("pub"));
        assert!(state.can_access("pub"));
    }

    #[test]
    fn recognizes_only_relay_authorized_private_session_metadata() {
        let f = Fixture::new();
        let mut state = f.state();
        state.accept(&f.member("work"), false);
        let session = |keys: &Keys, private: bool, at: u64| {
            let mut tags = vec![
                vec!["d".to_owned(), "work".to_owned()],
                vec!["name".to_owned(), "Work".to_owned()],
                vec!["t".to_owned(), "stream".to_owned()],
                vec!["about".to_owned(), SESSION_CHANNEL_DESCRIPTION.to_owned()],
            ];
            if private {
                tags.push(vec!["private".to_owned()]);
            }
            f.signed(keys, kinds::CHANNEL_METADATA, at, "", tags)
        };
        state.accept(&session(&f.viewer, true, 1), false);
        assert!(!state.is_session("work"));
        state.accept(&session(&f.relay, false, 1), false);
        assert!(!state.is_session("work"));
        state.accept(&session(&f.relay, true, 2), false);
        let channel = &state.channels()[0];
        assert_eq!(channel.channel_type, Some(ChannelType::Session));
        assert!(channel.private);
        assert_eq!(channel.description, None);
        assert_eq!(channel.updated_at, Some(2));
        state.accept(&f.metadata("work", 3, &[&["t", "session"]]), false);
        assert!(!state.is_session("work"));
        assert_eq!(state.get("work").unwrap().channel_type, None);
    }

    #[test]
    fn child_sessions_do_not_inherit_parent_access() {
        let f = Fixture::new();
        let mut state = f.state();
        let parent = "11111111-1111-4111-8111-111111111111";
        let child = "22222222-2222-4222-8222-222222222222";
        state.accept(&f.member(parent), false);
        let about = format!("{SESSION_CHANNEL_DESCRIPTION}\nparent:{parent}");
        state.accept(
            &f.metadata(
                child,
                T0,
                &[
                    &["t", "stream"],
                    &["private"],
                    &["name", "Work"],
                    &["about", &about],
                ],
            ),
            false,
        );
        assert!(!state.authorized(child));
        state.accept(&f.member(child), false);
        let summary = state.get(child).unwrap();
        assert_eq!(summary.channel_type, Some(ChannelType::Session));
        assert_eq!(summary.parent_channel_id.as_deref(), Some(parent));
        assert!(summary.private);
        state.accept(&f.roster(parent, &[], 1_800_000_000), false);
        assert!(!state.authorized(parent));
        assert!(state.authorized(child));
    }

    #[test]
    fn projects_metadata_like_the_reference_client() {
        let f = Fixture::new();
        let mut state = f.state();
        let viewer = f.viewer_hex();
        let other = Keys::generate().public_key().to_hex();
        state.accept(
            &f.roster(
                "dm",
                &[
                    viewer.clone(),
                    other.clone(),
                    other.clone(),
                    "not-hex".into(),
                ],
                T0,
            ),
            false,
        );
        state.accept(
            &f.metadata(
                "dm",
                T0,
                &[
                    &["name", "ignored"],
                    &["private"],
                    &["hidden"],
                    &["closed"],
                    &["t", "dm"],
                    &["archived", "true"],
                ],
            ),
            false,
        );
        let dm = state.get("dm").unwrap();
        let mut members = vec![viewer.clone(), other.clone()];
        members.sort();
        assert_eq!(dm.members, members);
        assert_eq!(dm.participants, Some(vec![other.clone()]));
        assert_eq!(dm.channel_type, Some(ChannelType::Dm));
        assert_eq!(dm.visibility, Some(Visibility::Private));
        assert!(dm.hidden && dm.private && dm.archived && !dm.read_only);
        assert_eq!(dm.description.as_deref(), Some(""));

        state.accept(&f.member("self"), false);
        state.accept(&f.metadata("self", T0, &[&["t", "dm"]]), false);
        assert_eq!(state.get("self").unwrap().participants, Some(vec![]));

        // No metadata: the name falls back to the id prefix.
        let id = "abcdef0123456789";
        state.accept(&f.member(id), false);
        let bare = state.get(id).unwrap();
        assert_eq!(bare.name, "abcdef01");
        assert_eq!(bare.description, None);
        assert_eq!(bare.visibility, None);

        // Content JSON `name` overrides the tag; both flags is unknown.
        let named = f.signed(
            &f.relay,
            kinds::CHANNEL_METADATA,
            T0,
            r#"{"name":"From content"}"#,
            vec![
                vec!["d".into(), "c".into()],
                vec!["name".into(), "From tag".into()],
                vec!["about".into(), "Chat".into()],
                vec!["public".into()],
                vec!["private".into()],
                vec!["t".into(), "workflow".into()],
            ],
        );
        state.accept(&f.member("c"), false);
        state.accept(&named, false);
        let c = state.get("c").unwrap();
        assert_eq!(c.name, "From content");
        assert_eq!(c.description.as_deref(), Some("Chat"));
        assert_eq!(c.visibility, None);
        assert_eq!(c.channel_type, None);
        assert!(!c.archived);
    }

    #[test]
    fn channels_are_joined_only_and_sorted_by_name() {
        let f = Fixture::new();
        let mut state = f.state();
        for (id, name) in [("3", "beta"), ("2", "Alpha"), ("1", "alpha")] {
            state.accept(&f.member(id), false);
            state.accept(&f.named(id, name), false);
        }
        state.accept(
            &f.metadata("pub", T0, &[&["public"], &["name", "aaa"]]),
            false,
        );
        let ids: Vec<String> = state.channels().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["1", "2", "3"]);
        let saved = state.saved_events();
        assert_eq!(saved.len(), 6);
    }

    #[test]
    fn single_short_page_certifies_and_requests_missing_names() {
        let f = Fixture::new();
        let mut state = f.state();
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        let read = scan.next_read().unwrap();
        assert_eq!(
            Value::Object(read.filter),
            json!({"kinds": [39002], "#p": [f.viewer_hex()], "limit": 500})
        );
        scan.accept(
            &mut state,
            &[f.member("a"), f.member("b"), f.named("b", "Bee")],
        )
        .unwrap();
        assert!(scan.certified());
        assert_eq!(scan.stage(), ScanStage::Metadata);
        let read = scan.next_read().unwrap();
        assert_eq!(
            Value::Object(read.filter),
            json!({"kinds": [39000], "#d": ["a"], "limit": 500})
        );
        scan.accept(&mut state, &[f.named("a", "Ay")]).unwrap();
        assert_eq!(scan.stage(), ScanStage::Done);
        let names: Vec<String> = state.channels().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["Ay", "Bee"]);
    }

    #[test]
    fn strong_scans_ask_for_strong_reads_everywhere() {
        let f = Fixture::new();
        let state = f.state();
        let scan = RosterScan::new(
            &state,
            ScanOptions {
                strong: true,
                force_metadata: false,
            },
        );
        assert_eq!(
            scan.next_read().unwrap().filter["consistency"],
            json!("strong")
        );
        let filter = confirmation_filter("r", "v", &["a".into()], true);
        assert_eq!(filter["consistency"], json!("strong"));
        assert_eq!(
            metadata_filter(&["a".into()], true)["consistency"],
            json!("strong")
        );
    }

    #[test]
    fn exhaustion_read_and_omitted_confirmation_after_a_full_page() {
        let f = Fixture::new();
        let all = memberships(&f, 500);
        let mut state = f.state();
        state.accept(&f.member("omitted"), false);
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        scan.next_read().unwrap();
        scan.accept(&mut state, &all).unwrap();
        assert!(state.authorized("omitted"));
        let last = all.last().unwrap();
        let exhaustion = scan.next_read().unwrap().filter;
        assert_eq!(exhaustion["until"], json!(last.created_at.as_secs()));
        assert_eq!(exhaustion["before_id"], json!(last.id.to_hex()));
        scan.accept(&mut state, &[]).unwrap();
        assert_eq!(scan.stage(), ScanStage::Confirm);
        let confirm = scan.next_read().unwrap();
        assert!(confirm.fresh);
        assert_eq!(
            Value::Object(confirm.filter),
            json!({
                "kinds": [39002],
                "authors": [f.relay_hex()],
                "#d": ["omitted"],
                "#p": [f.viewer_hex()],
                "limit": 2,
            })
        );
        // Still a member until the exact read says otherwise.
        assert!(state.authorized("omitted"));
        scan.accept(&mut state, &[]).unwrap();
        assert!(scan.certified());
        assert!(!state.authorized("omitted"));
        assert_eq!(scan.stage(), ScanStage::Metadata);
        let mut requested = 0;
        while let Some(read) = scan.next_read() {
            let ids = read.filter["#d"].as_array().unwrap().len();
            assert!(ids <= 500);
            requested += ids;
            scan.accept(&mut state, &[]).unwrap();
        }
        assert_eq!(requested, 500);
        assert_eq!(state.channels().len(), 500);
    }

    #[test]
    fn a_membership_that_moved_ahead_of_the_cursor_is_confirmed() {
        let f = Fixture::new();
        let viewer = f.viewer_hex();
        let mut state = f.state();
        state.accept(
            &f.roster("omitted", std::slice::from_ref(&viewer), T0 - 1),
            false,
        );
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        scan.accept(&mut state, &memberships(&f, 500)).unwrap();
        scan.accept(&mut state, &[]).unwrap();
        let fresh = f.roster("omitted", std::slice::from_ref(&viewer), T0 + 1);
        scan.accept(&mut state, &[fresh]).unwrap();
        assert!(scan.certified());
        assert!(state.authorized("omitted"));
    }

    #[test]
    fn confirms_omitted_memberships_in_128_channel_batches() {
        let f = Fixture::new();
        let viewer = f.viewer_hex();
        let mut state = f.state();
        let omitted: Vec<String> = (0..129).map(|i| format!("omitted-{i}")).collect();
        let events: Vec<Verified> = omitted.iter().map(|id| f.member(id)).collect();
        state.apply(&events, None, None, false);
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        scan.accept(&mut state, &memberships(&f, 500)).unwrap();
        scan.accept(&mut state, &[]).unwrap();
        let mut seen = Vec::new();
        for expected in [129, 2] {
            let read = scan.next_read().unwrap().filter;
            assert_eq!(read["limit"], json!(expected));
            let ids: Vec<String> = read["#d"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(ids.len(), expected - 1);
            let fresh: Vec<Verified> = ids
                .iter()
                .map(|id| f.roster(id, std::slice::from_ref(&viewer), T0 + 1))
                .collect();
            seen.extend(ids);
            scan.accept(&mut state, &fresh).unwrap();
        }
        seen.sort();
        let mut want = omitted.clone();
        want.sort();
        assert_eq!(seen, want);
        assert!(scan.certified());
        assert_eq!(state.channels().len(), 629);
    }

    #[test]
    fn live_grants_between_pages_survive_the_scan() {
        let f = Fixture::new();
        let viewer = f.viewer_hex();
        let mut state = f.state();
        state.apply(
            &[
                f.roster("renewed", std::slice::from_ref(&viewer), 10),
                f.roster("omitted", std::slice::from_ref(&viewer), 10),
            ],
            None,
            None,
            false,
        );
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        state.accept(
            &f.roster("renewed", std::slice::from_ref(&viewer), 11),
            false,
        );
        let all = memberships(&f, 501);
        scan.accept(&mut state, &all[..500]).unwrap();
        state.accept(
            &f.roster("new-grant", std::slice::from_ref(&viewer), 12),
            false,
        );
        scan.accept(&mut state, &all[500..]).unwrap();
        // Only the unchanged scan-start membership needs confirmation.
        let confirm = scan.next_read().unwrap().filter;
        assert_eq!(confirm["#d"], json!(["omitted"]));
        scan.accept(&mut state, &[]).unwrap();
        assert!(state.authorized("renewed"));
        assert!(state.authorized("new-grant"));
        assert!(!state.authorized("omitted"));
        assert_eq!(state.channels().len(), 503);
    }

    #[test]
    fn continues_into_an_older_timestamp_after_a_full_same_timestamp_page() {
        let f = Fixture::new();
        let viewer = f.viewer_hex();
        let mut state = f.state();
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        scan.accept(&mut state, &memberships(&f, 500)).unwrap();
        scan.accept(
            &mut state,
            &[f.roster("older", std::slice::from_ref(&viewer), T0 - 1)],
        )
        .unwrap();
        assert!(scan.certified());
        assert_eq!(state.channels().len(), 501);
    }

    #[test]
    fn a_repeated_page_is_an_invalid_response() {
        for repeat in [500, 1] {
            let f = Fixture::new();
            let all = memberships(&f, 500);
            let mut state = f.state();
            let mut scan = RosterScan::new(&state, ScanOptions::default());
            scan.accept(&mut state, &all).unwrap();
            let error = scan.accept(&mut state, &all[..repeat]).unwrap_err();
            assert_eq!(
                error,
                DiscoveryError::InvalidResponse("Channel discovery cursor did not advance")
            );
            assert_eq!(scan.stage(), ScanStage::Done);
            assert!(!scan.certified());
            // Partial grants are kept.
            assert_eq!(state.channels().len(), 500);
        }
    }

    #[test]
    fn confirmation_answers_are_checked() {
        let f = Fixture::new();
        let viewer = f.viewer_hex();
        let mut state = f.state();
        state.accept(&f.member("omitted"), false);
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        scan.accept(&mut state, &memberships(&f, 500)).unwrap();
        scan.accept(&mut state, &[]).unwrap();
        let stray = f.roster("elsewhere", &[viewer], T0 + 1);
        assert!(scan.accept(&mut state, &[stray]).is_err());
        assert!(!scan.certified());
        assert!(state.authorized("omitted"));
    }

    #[test]
    fn capacity_overflow_never_revokes_by_omission() {
        let f = Fixture::new();
        let all = memberships(&f, 1025);
        let mut state = f.state();
        state.accept(&f.member("omitted"), false);
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        scan.accept(&mut state, &all[..500]).unwrap();
        scan.accept(&mut state, &all[500..1000]).unwrap();
        scan.accept(&mut state, &all[1000..]).unwrap();
        assert!(!scan.certified());
        assert_eq!(scan.stage(), ScanStage::Metadata);
        assert!(state.authorized("omitted"));
        assert_eq!(state.channels().len(), 1024);
    }

    #[test]
    fn page_budget_bounds_a_relay_repeating_coordinates() {
        let f = Fixture::new();
        let viewer = f.viewer_hex();
        let mut state = f.state();
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        // Each page re-sends the same 500 channels with older versions.
        for age in 0..3 {
            let mut page: Vec<Verified> = (0..500)
                .map(|i| f.roster(&uuid(i), std::slice::from_ref(&viewer), T0 - age))
                .collect();
            page.sort_by_key(|e| e.id);
            assert_eq!(scan.stage(), ScanStage::Roster);
            scan.accept(&mut state, &page).unwrap();
        }
        assert_eq!(scan.stage(), ScanStage::Metadata);
        assert!(!scan.certified());
        assert!(!state.is_complete());
    }

    #[test]
    fn denial_while_reading_rosters_denies_everything() {
        let f = Fixture::new();
        let mut state = f.state();
        state.accept(&f.member("a"), false);
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        assert!(scan.fail(&mut state, true));
        assert!(!state.can_access("a"));

        let mut state = f.state();
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        scan.accept(&mut state, &[f.member("a")]).unwrap();
        assert_eq!(scan.stage(), ScanStage::Metadata);
        // Failed names do not discard membership.
        assert!(!scan.fail(&mut state, true));
        assert!(state.authorized("a"));
    }

    #[test]
    fn cached_metadata_is_read_again() {
        let f = Fixture::new();
        let mut state = f.state();
        state.apply(&[f.member("a"), f.named("a", "Old")], None, None, true);
        assert!(state.pending_metadata("a"));
        let mut scan = RosterScan::new(&state, ScanOptions::default());
        scan.accept(&mut state, &[f.member("a")]).unwrap();
        assert_eq!(scan.next_read().unwrap().filter["#d"], json!(["a"]));
        scan.accept(&mut state, &[f.named("a", "Old")]).unwrap();
        assert!(!state.pending_metadata("a"));
        let mut again = RosterScan::new(&state, ScanOptions::default());
        again.accept(&mut state, &[f.member("a")]).unwrap();
        assert_eq!(again.stage(), ScanStage::Done);
    }

    #[test]
    fn exact_resolution_denies_unconfirmed_cache_and_vanished_previews() {
        let f = Fixture::new();
        let mut state = f.state();
        state.apply(&[f.member("cached")], None, None, true);
        state.accept(&f.metadata("pub", T0, &[&["public"]]), false);
        let ids = state
            .resolve_targets(&["cached".into(), "pub".into(), "cached".into()])
            .unwrap();
        assert_eq!(ids, ["cached", "pub"]);
        let filters = resolve_filters(&f.relay_hex(), &f.viewer_hex(), &ids, false);
        assert_eq!(filters[0]["limit"], json!(3));
        assert_eq!(filters[1]["#p"], json!([f.viewer_hex()]));
        let start = state.resolution_start(&ids);
        let resolution = state.apply_resolution(&ids, &start, &[]).unwrap();
        assert_eq!(resolution.denied, ["cached", "pub"]);
        assert!(!state.can_access("cached"));
        assert!(!state.can_access("pub"));

        let mut state = f.state();
        state.apply(&[f.member("cached")], None, None, true);
        let ids = vec!["cached".to_owned()];
        let start = state.resolution_start(&ids);
        let resolution = state
            .apply_resolution(
                &ids,
                &start,
                &[f.member("cached"), f.named("cached", "Back")],
            )
            .unwrap();
        assert!(resolution.denied.is_empty());
        assert!(state.can_participate("cached"));

        let too_many = [f.named("cached", "a"), f.named("cached", "b")];
        assert!(state.apply_resolution(&ids, &start, &too_many).is_err());
        let ids: Vec<String> = (0..129).map(|i| i.to_string()).collect();
        assert_eq!(
            state.resolve_targets(&ids),
            Err(DiscoveryError::TooManyChannels)
        );
    }

    #[test]
    fn membership_hints_are_relay_signed_and_addressed_to_the_viewer() {
        let f = Fixture::new();
        let viewer = f.viewer_hex();
        let relay = f.relay_hex();
        let hint =
            |keys: &Keys, kind: u16, tags: Vec<Vec<String>>| f.signed(keys, kind, T0, "", tags);
        let p = vec!["p".to_owned(), viewer.clone()];
        let h = |id: &str| vec!["h".to_owned(), id.to_owned()];
        assert_eq!(
            membership_hint(
                &hint(&f.relay, kinds::MEMBER_ADDED, vec![p.clone(), h("c")]),
                &viewer,
                &relay
            ),
            Some(MembershipHint::Resolve("c".into()))
        );
        assert_eq!(
            membership_hint(
                &hint(
                    &f.relay,
                    kinds::MEMBER_ADDED,
                    vec![p.clone(), h("a"), h("b")]
                ),
                &viewer,
                &relay
            ),
            Some(MembershipHint::Refresh)
        );
        assert_eq!(
            membership_hint(
                &hint(&f.relay, kinds::MEMBER_REMOVED, vec![p.clone(), h("c")]),
                &viewer,
                &relay
            ),
            Some(MembershipHint::Refresh)
        );
        assert_eq!(
            membership_hint(
                &hint(&f.viewer, kinds::MEMBER_ADDED, vec![p, h("c")]),
                &viewer,
                &relay
            ),
            None
        );
        assert_eq!(
            membership_hint(
                &hint(
                    &f.relay,
                    kinds::MEMBER_ADDED,
                    vec![vec!["p".into(), relay.clone()], h("c")]
                ),
                &viewer,
                &relay
            ),
            None
        );
        assert_eq!(
            Value::Object(membership_hint_filter(&viewer)),
            json!({"kinds": [44100, 44101], "#p": [viewer]})
        );
    }

    #[test]
    fn membership_changes_need_the_relay_and_valid_pubkeys() {
        let f = Fixture::new();
        let relay = f.relay_hex();
        let actor = Keys::generate().public_key().to_hex();
        let target = Keys::generate().public_key().to_hex();
        let row = |keys: &Keys, body: Value| {
            f.signed(
                keys,
                kinds::SYSTEM_MESSAGE,
                T0,
                &body.to_string(),
                vec![vec!["h".into(), "c".into()]],
            )
        };
        let joined = row(
            &f.relay,
            json!({"type": "member_joined", "actor": actor, "target": target}),
        );
        assert_eq!(
            membership_change(&joined, &relay),
            Some(MembershipChange {
                kind: MembershipChangeType::Joined,
                actor: actor.clone(),
                target: target.clone()
            })
        );
        let left = row(&f.relay, json!({"type": "member_left", "actor": actor}));
        assert_eq!(membership_change(&left, &relay).unwrap().target, actor);
        let left_other = row(
            &f.relay,
            json!({"type": "member_left", "actor": actor, "target": target}),
        );
        assert_eq!(membership_change(&left_other, &relay), None);
        let forged = row(
            &f.viewer,
            json!({"type": "member_joined", "actor": actor, "target": target}),
        );
        assert_eq!(membership_change(&forged, &relay), None);
        let bad = row(
            &f.relay,
            json!({"type": "member_removed", "actor": actor, "target": "x"}),
        );
        assert_eq!(membership_change(&bad, &relay), None);
    }

    #[test]
    fn session_metadata_needs_the_exact_marker() {
        assert_eq!(
            session_metadata(Some(SESSION_CHANNEL_DESCRIPTION)),
            Some(None)
        );
        let parent = "11111111-1111-4111-8111-111111111111";
        assert_eq!(
            session_metadata(Some(&format!(
                "{SESSION_CHANNEL_DESCRIPTION}\nparent:{parent}"
            ))),
            Some(Some(parent.to_owned()))
        );
        assert_eq!(
            session_metadata(Some(&format!("{SESSION_CHANNEL_DESCRIPTION}\nparent:nope"))),
            None
        );
        assert_eq!(session_metadata(Some("Buzz session")), None);
        assert_eq!(session_metadata(None), None);
    }
}
