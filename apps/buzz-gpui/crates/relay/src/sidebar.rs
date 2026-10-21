//! The channel sidebar: sections and sorting, DM labels, hidden DMs, the
//! channel-activity inputs of "recent" sorting, and the kind-30078 sidebar
//! preferences (`docs/port/relay.md` §5a.4–5a.9).
//!
//! Everything here is pure. The session runs the reads and publications;
//! this module builds their filters and templates, decodes and validates
//! what comes back, and computes the edits.
//!
//! Preferences are four NIP-44 self-encrypted records with no community
//! scoping in their `d` tags (§9.3 #18): `channel-sections`,
//! `channel-stars`, `channel-mutes` and `channel-sort`. Sections and sort
//! carry Desktop-compatible registers in `meta`, which are authoritative;
//! the legacy top-level fields are their projection.
//!
//! Ported from `src/bundled/channels/sidebar-sections.ts`,
//! `useChannelLabels.ts`, `useHiddenDms.ts`,
//! `src/features/relay/sidebar-{preferences,registers,edits}.ts`,
//! `native-sidebar.ts`, `channel-activity.ts`, and the sidebar commands of
//! `src-tauri/src/identity.rs` (338-750).

use crate::discovery::{ChannelSummary, ChannelType, is_uuid, locale_compare};
use crate::event::{Verified, kinds};
use crate::read_state::is_channel_key;
use crate::socket::{Filter, filter as object};
use buzz_identity::{Identity, IdentityError, Purpose};
use nostr::prelude::{SecretKey, UnsignedEvent};
use serde_json::{Map, Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Sections (§5a.4)
// ---------------------------------------------------------------------------

/// One rendered sidebar section.
#[derive(Clone, Debug, PartialEq)]
pub struct SidebarSection<'a> {
    /// `starred`, `group:<sectionId>`, `channels`, `forums` or `dms`.
    pub key: String,
    pub title: String,
    pub icon: Option<String>,
    pub rows: Vec<&'a ChannelSummary>,
}

impl SidebarSection<'_> {
    /// The key its sort preference is stored under: `section:<id>` for
    /// saved groups, the section key otherwise.
    pub fn sort_key(&self) -> String {
        match self.key.strip_prefix("group:") {
            Some(id) => format!("section:{id}"),
            None => self.key.clone(),
        }
    }
}

/// Sections where a new channel can be created: `channels` and groups.
pub fn is_channel_section_key(key: &str) -> bool {
    key == "channels" || key.starts_with("group:")
}

/// Sections a channel can be dragged into.
pub fn is_channel_drop_section_key(key: &str) -> bool {
    key == "starred" || is_channel_section_key(key)
}

/// Archived channels are never shown; `hidden` hides everything but DMs.
fn active(channel: &ChannelSummary) -> bool {
    !channel.archived && (!channel.hidden || channel.channel_type == Some(ChannelType::Dm))
}

/// Whether [`sidebar_sections`] lists `channel`: active, and not a DM the
/// viewer hid.
pub fn listed(channel: &ChannelSummary, hidden_dms: &HashSet<String>) -> bool {
    active(channel) && !hidden_dms.contains(&channel.id)
}

/// JavaScript `<` on lowercased strings compares UTF-16 code units.
fn compare_name(a: &ChannelSummary, b: &ChannelSummary) -> Ordering {
    let (x, y) = (a.name.to_lowercase(), b.name.to_lowercase());
    x.encode_utf16()
        .cmp(y.encode_utf16())
        .then_with(|| locale_compare(&a.id, &b.id))
}

fn sort_rows(rows: &mut [&ChannelSummary], mode: SortMode) {
    rows.sort_by(|a, b| {
        if mode == SortMode::Recent {
            match (a.last_activity_at, b.last_activity_at) {
                (Some(x), Some(y)) if x != y => return y.cmp(&x),
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                _ => {}
            }
        }
        compare_name(a, b)
    });
}

/// Arranges the authorized roster into sections
/// (`sidebar-sections.ts:13-117`). Preferences only arrange the supplied
/// channels; they never add any. The caller passes the roster after DM
/// labelling and without channels placed in "Me" (§5a.8), and the union
/// of relay-hidden and locally hidden DMs (§5a.5).
pub fn sidebar_sections<'a>(
    channels: &'a [ChannelSummary],
    preferences: Option<&SidebarPreferences>,
    hidden_dms: &HashSet<String>,
) -> Vec<SidebarSection<'a>> {
    let active: Vec<&ChannelSummary> = channels.iter().filter(|c| active(c)).collect();
    let is = |c: &ChannelSummary, t: ChannelType| c.channel_type == Some(t);
    let streams: Vec<&ChannelSummary> = active
        .iter()
        .copied()
        .filter(|c| !is(c, ChannelType::Dm) && !is(c, ChannelType::Forum))
        .collect();
    let dms: Vec<&ChannelSummary> = active
        .iter()
        .copied()
        .filter(|c| is(c, ChannelType::Dm) && !hidden_dms.contains(&c.id))
        .collect();
    let movable: Vec<&ChannelSummary> = streams.iter().chain(dms.iter()).copied().collect();
    let stars: HashSet<&str> = preferences
        .map(|p| p.starred.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let groups = preferences
        .map(|p| p.sections.as_slice())
        .unwrap_or_default();
    let group_ids: HashSet<&str> = groups.iter().map(|g| g.id.as_str()).collect();
    let assignment = |id: &str| {
        preferences
            .and_then(|p| p.assignments.get(id))
            .map(String::as_str)
    };
    let unplaced = |c: &&ChannelSummary| {
        !stars.contains(c.id.as_str()) && !assignment(&c.id).is_some_and(|g| group_ids.contains(g))
    };
    let section = |key: String,
                   title: &str,
                   icon: Option<String>,
                   sort: &str,
                   mut rows: Vec<&'a ChannelSummary>| {
        sort_rows(
            &mut rows,
            preferences.map(|p| p.sort_mode(sort)).unwrap_or_default(),
        );
        SidebarSection {
            key,
            title: title.to_owned(),
            icon,
            rows,
        }
    };
    let mut sections = vec![section(
        "starred".into(),
        "Starred",
        Some("★".into()),
        "starred",
        movable
            .iter()
            .copied()
            .filter(|c| stars.contains(c.id.as_str()))
            .collect(),
    )];
    for group in groups {
        sections.push(section(
            format!("group:{}", group.id),
            &group.name,
            group.icon.clone(),
            &format!("section:{}", group.id),
            movable
                .iter()
                .copied()
                .filter(|c| {
                    !stars.contains(c.id.as_str()) && assignment(&c.id) == Some(group.id.as_str())
                })
                .collect(),
        ));
    }
    sections.push(section(
        "channels".into(),
        "Channels",
        None,
        "channels",
        streams.iter().copied().filter(unplaced).collect(),
    ));
    sections.push(section(
        "forums".into(),
        "Forums",
        None,
        "forums",
        active
            .iter()
            .copied()
            .filter(|c| is(c, ChannelType::Forum))
            .collect(),
    ));
    sections.push(section(
        "dms".into(),
        "Direct messages",
        None,
        "dms",
        dms.iter().copied().filter(unplaced).collect(),
    ));
    sections.retain(|s| is_channel_section_key(&s.key) || s.key == "dms" || !s.rows.is_empty());
    sections
}

/// Does any section sort by recent activity? Only then is channel activity
/// read at startup (`useSidebarStartup.ts:69-74`).
pub fn needs_activity(preferences: &SidebarPreferences) -> bool {
    preferences
        .sort
        .values()
        .any(|mode| *mode == SortMode::Recent)
}

/// Removes channels placed in "Me" from the main sidebar
/// (`ChannelSidebar.tsx:293-301`).
pub fn without_me_channels(channels: &[ChannelSummary], me: &MeGroups) -> Vec<ChannelSummary> {
    let me: HashSet<&str> = me.channels.iter().map(String::as_str).collect();
    channels
        .iter()
        .filter(|c| !me.contains(c.id.as_str()))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Labels (§5a.9)
// ---------------------------------------------------------------------------

/// Most DM participants whose profiles the sidebar asks for at once.
pub const LABEL_PROFILE_LIMIT: usize = 1024;

/// Participants of visible DMs whose names the sidebar needs, sorted and
/// capped like `useChannelLabels.ts`.
pub fn label_profile_ids(roster: &[ChannelSummary]) -> Vec<String> {
    let mut ids: Vec<String> = roster
        .iter()
        .filter(|c| active(c))
        .flat_map(|c| c.participants.iter().flatten().cloned())
        .collect();
    ids.sort();
    ids.dedup();
    ids.truncate(LABEL_PROFILE_LIMIT);
    ids
}

/// Visible channels with DM names replaced by participant names
/// (`useChannelLabels.ts:1-77`). `name_of(pubkey, participants)` returns
/// the resolved name, if any; otherwise the first 10 hex characters are
/// shown. A DM with no other participants is "Notes to self".
pub fn label_channels(
    roster: &[ChannelSummary],
    mut name_of: impl FnMut(&str, &[String]) -> Option<String>,
) -> Vec<ChannelSummary> {
    roster
        .iter()
        .filter(|c| active(c))
        .map(|channel| {
            let (Some(ChannelType::Dm), Some(participants)) =
                (channel.channel_type, &channel.participants)
            else {
                return channel.clone();
            };
            let name = if participants.is_empty() {
                "Notes to self".to_owned()
            } else {
                participants
                    .iter()
                    .map(|id| {
                        name_of(id, participants).unwrap_or_else(|| id.chars().take(10).collect())
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            ChannelSummary {
                name,
                ..channel.clone()
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Hidden DMs (§5a.5)
// ---------------------------------------------------------------------------

/// View-state key of the local "Remove from Messages" list.
pub const HIDDEN_DMS_VIEW_KEY: &str = "hidden-dms";
/// History probe attempts per hidden DM; the wait before retry `n` is
/// `500 · 2^n` ms.
pub const HIDDEN_DM_PROBE_ATTEMPTS: u32 = 3;

pub fn hidden_dm_probe_backoff_ms(attempt: u32) -> u64 {
    500 << attempt
}

/// The sidebar hides the union of relay-hidden DMs (30622, §5e.5) and
/// locally hidden ones.
pub fn hidden_dm_union(relay: &HashSet<String>, local: &LocalHiddenDms) -> HashSet<String> {
    relay.iter().cloned().chain(local.ids()).collect()
}

/// The newest message of a channel, as unread tracking reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageHead {
    pub id: String,
    pub created_at: u64,
}

/// What the channel looked like when it was hidden.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Baseline {
    /// Not known yet (stored as an absent field).
    Unknown,
    /// The channel had no messages (stored as `null`).
    Empty,
    Head(MessageHead),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HiddenDm {
    pub id: String,
    /// Identifies one hide, so a send that started before a re-hide does
    /// not undo it. Not persisted.
    pub generation: u64,
    pub baseline: Baseline,
    pub known_ids: Option<Vec<String>>,
}

/// What a history probe did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// A message not seen before arrived: the DM is shown again.
    Shown,
    /// First probe: the window and baseline were recorded.
    Recorded,
    Unchanged,
}

/// The local, viewer-scoped "Remove from Messages" list
/// (`useHiddenDms.ts`). Verified message evidence restores a row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalHiddenDms {
    entries: Vec<HiddenDm>,
    next_generation: u64,
}

impl LocalHiddenDms {
    /// Restores the saved JSON list, skipping malformed entries.
    pub fn restore(saved: &Value) -> Self {
        let mut list = Self::default();
        for entry in saved.as_array().into_iter().flatten() {
            let Some(id) = entry.get("id").and_then(Value::as_str) else {
                continue;
            };
            let baseline = match entry.get("baseline") {
                Some(Value::Null) => Baseline::Empty,
                Some(head) => match (
                    head.get("id").and_then(Value::as_str),
                    head.get("createdAt").and_then(Value::as_u64),
                ) {
                    (Some(id), Some(created_at)) => Baseline::Head(MessageHead {
                        id: id.to_owned(),
                        created_at,
                    }),
                    _ => Baseline::Unknown,
                },
                None => Baseline::Unknown,
            };
            let known_ids = entry
                .get("knownIds")
                .and_then(Value::as_array)
                .and_then(|ids| {
                    ids.iter()
                        .map(|id| id.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                });
            let generation = list.generation();
            list.entries.push(HiddenDm {
                id: id.to_owned(),
                generation,
                baseline,
                known_ids,
            });
        }
        list
    }

    pub fn to_json(&self) -> Value {
        Value::Array(
            self.entries
                .iter()
                .map(|entry| {
                    let mut object = Map::new();
                    object.insert("id".into(), json!(entry.id));
                    match &entry.baseline {
                        Baseline::Unknown => {}
                        Baseline::Empty => {
                            object.insert("baseline".into(), Value::Null);
                        }
                        Baseline::Head(head) => {
                            object.insert(
                                "baseline".into(),
                                json!({"id": head.id, "createdAt": head.created_at}),
                            );
                        }
                    }
                    if let Some(ids) = &entry.known_ids {
                        object.insert("knownIds".into(), json!(ids));
                    }
                    Value::Object(object)
                })
                .collect(),
        )
    }

    fn generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    pub fn entries(&self) -> &[HiddenDm] {
        &self.entries
    }

    pub fn ids(&self) -> impl Iterator<Item = String> + '_ {
        self.entries.iter().map(|entry| entry.id.clone())
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }

    /// Hides a DM. `latest` is the channel's current unread head.
    pub fn hide(&mut self, id: &str, latest: Option<MessageHead>) {
        self.entries.retain(|entry| entry.id != id);
        let generation = self.generation();
        self.entries.push(HiddenDm {
            id: id.to_owned(),
            generation,
            baseline: latest.map_or(Baseline::Unknown, Baseline::Head),
            known_ids: None,
        });
    }

    /// Shows DMs again: opened through the DM picker, or a live message
    /// arrived in them. Returns whether anything changed.
    pub fn show(&mut self, ids: &[&str]) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|entry| !ids.contains(&entry.id.as_str()));
        self.entries.len() != before
    }

    /// The hide generation now in force for `id`, captured when the viewer
    /// sends a message (kind 9/40002/40008 with that single `h`).
    pub fn generation_of(&self, id: &str) -> Option<u64> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.generation)
    }

    /// The viewer's send completed: show the DM unless it was hidden again
    /// meanwhile.
    pub fn sent(&mut self, id: &str, generation_at_send: u64) -> bool {
        if self.generation_of(id) == Some(generation_at_send) {
            self.show(&[id])
        } else {
            false
        }
    }

    /// Shows DMs whose unread head moved past the hide baseline. Returns
    /// whether anything changed.
    pub fn check_heads(&mut self, mut latest: impl FnMut(&str) -> Option<MessageHead>) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| {
            let Some(latest) = latest(&entry.id) else {
                return true;
            };
            match &entry.baseline {
                Baseline::Unknown => true,
                Baseline::Empty => false,
                Baseline::Head(baseline) => {
                    latest.id == baseline.id
                        || !(latest.created_at > baseline.created_at
                            || (latest.created_at == baseline.created_at
                                && latest.id < baseline.id))
                }
            }
        });
        self.entries.len() != before
    }

    /// The bounded history read for a hidden DM that is in the ready DM
    /// roster.
    pub fn probe_filter(&self, id: &str) -> Option<Filter> {
        let entry = self.entries.iter().find(|entry| entry.id == id)?;
        Some(object(json!({
            "kinds": [kinds::CHAT, kinds::AGENT_MESSAGE, kinds::DIFF],
            "#h": [id],
            "limit": if entry.known_ids.is_some() { 50 } else { 100 },
        })))
    }

    /// Applies a probe answer for the hide `generation` it was started for.
    pub fn apply_probe(
        &mut self,
        id: &str,
        generation: u64,
        event_ids: &[String],
        latest: Option<MessageHead>,
    ) -> ProbeOutcome {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.id == id && e.generation == generation)
        else {
            return ProbeOutcome::Unchanged;
        };
        if let Some(known) = &entry.known_ids
            && event_ids.iter().any(|id| !known.contains(id))
        {
            self.show(&[id]);
            return ProbeOutcome::Shown;
        }
        if entry.baseline == Baseline::Unknown || entry.known_ids.is_none() {
            entry.baseline = latest.map_or(Baseline::Empty, Baseline::Head);
            entry.known_ids = Some(event_ids.to_vec());
            return ProbeOutcome::Recorded;
        }
        ProbeOutcome::Unchanged
    }
}

// ---------------------------------------------------------------------------
// Channel activity for "recent" sorting (§5a.4, §5i.11)
// ---------------------------------------------------------------------------

/// Kinds that count as channel activity.
pub const ACTIVITY_KINDS: &[u16] = &[
    kinds::CHAT,
    kinds::AGENT_MESSAGE,
    kinds::DIFF,
    kinds::FORUM_POST,
    kinds::FORUM_COMMENT,
];
/// Channels per activity query.
pub const ACTIVITY_BATCH: usize = 128;

/// One query for up to [`ACTIVITY_BATCH`] channels: one `limit: 1` filter
/// per channel (`native.ts:813-829`). This exceeds the reader's 4-filter
/// budget, so the session sends it on the raw `/query` path with
/// background priority and a 10 s timeout, as the reference does.
pub fn activity_filters(channel_ids: &[String]) -> Result<Vec<Filter>, SidebarError> {
    if channel_ids.is_empty()
        || channel_ids.len() > ACTIVITY_BATCH
        || !channel_ids.iter().all(|id| is_channel_key(id))
    {
        return Err(SidebarError::Invalid("Activity filter rejected"));
    }
    Ok(channel_ids
        .iter()
        .map(|id| object(json!({"kinds": ACTIVITY_KINDS, "#h": [id], "limit": 1})))
        .collect())
}

/// Newest activity per channel for "recent" sorting
/// (`channel-activity.ts`). Authoritative refreshes may clear values that
/// did not change during the read; live events only advance them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelActivity {
    values: HashMap<String, u64>,
}

/// Collected answers of one refresh.
#[derive(Clone, Debug, Default)]
pub struct ActivityRefresh {
    at_start: HashMap<String, u64>,
    refreshed: HashMap<String, u64>,
}

impl ActivityRefresh {
    /// Takes the answer to [`activity_filters`] for `batch`.
    pub fn collect(&mut self, batch: &[String], events: &[Verified]) {
        for event in events {
            let Some(id) = event.channel() else {
                continue;
            };
            let at = event.created_at_secs();
            if batch.iter().any(|b| b == id)
                && ACTIVITY_KINDS.contains(&event.kind_u16())
                && self.refreshed.get(id).is_none_or(|v| at > *v)
            {
                self.refreshed.insert(id.to_owned(), at);
            }
        }
    }
}

impl ChannelActivity {
    pub fn last(&self, id: &str) -> Option<u64> {
        self.values.get(id).copied()
    }

    /// Live verified events: only advance. Returns whether anything changed.
    pub fn accept(&mut self, events: &[Verified]) -> bool {
        let mut changed = false;
        for event in events {
            if !ACTIVITY_KINDS.contains(&event.kind_u16()) {
                continue;
            }
            let Some(id) = event.channel() else {
                continue;
            };
            let at = event.created_at_secs();
            if self.values.get(id).is_none_or(|v| at > *v) {
                self.values.insert(id.to_owned(), at);
                changed = true;
            }
        }
        changed
    }

    pub fn begin_refresh(&self) -> ActivityRefresh {
        ActivityRefresh {
            at_start: self.values.clone(),
            refreshed: HashMap::new(),
        }
    }

    /// Commits a finished refresh of `ids`. Values for other channels are
    /// dropped; values that live traffic advanced during the read are kept.
    pub fn commit(&mut self, ids: &[String], refresh: ActivityRefresh) {
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut next = self.values.clone();
        for id in ids {
            let displayed = self.values.get(id).copied();
            let result = refresh.refreshed.get(id).copied();
            let changed_during_read = displayed != refresh.at_start.get(id).copied();
            if let Some(displayed) = displayed
                && match result {
                    Some(result) => displayed > result,
                    None => changed_during_read,
                }
            {
                continue;
            }
            match result {
                Some(result) => next.insert(id.clone(), result),
                None => next.remove(id),
            };
        }
        next.retain(|id, _| wanted.contains(id.as_str()));
        self.values = next;
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }

    /// Copies values into `last_activity_at`.
    pub fn annotate(&self, channels: &mut [ChannelSummary]) {
        for channel in channels {
            channel.last_activity_at = self.last(&channel.id);
        }
    }
}

// ---------------------------------------------------------------------------
// Preferences: model and projection (§5a.6–5a.8)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SidebarError {
    #[error("{0}")]
    Invalid(&'static str),
    /// The readback did not show the requested intent: another device or
    /// session wrote in between.
    #[error("{0}")]
    Conflict(&'static str),
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

fn invalid<T>(message: &'static str) -> Result<T, SidebarError> {
    Err(SidebarError::Invalid(message))
}

/// The four preference coordinates, used as both `d` and `t` tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Coordinate {
    Sections,
    Stars,
    Mutes,
    Sort,
}

impl Coordinate {
    pub const ALL: [Coordinate; 4] = [Self::Sections, Self::Stars, Self::Mutes, Self::Sort];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sections => "channel-sections",
            Self::Stars => "channel-stars",
            Self::Mutes => "channel-mutes",
            Self::Sort => "channel-sort",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == value)
    }

    /// The record a missing head stands for.
    pub fn empty(self) -> Map<String, Value> {
        object(match self {
            Self::Sections => json!({"version": 1, "sections": [], "assignments": {}}),
            Self::Sort => json!({"version": 1, "groups": {}}),
            Self::Stars | Self::Mutes => json!({"version": 1, "channels": {}}),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SortMode {
    #[default]
    Alpha,
    Recent,
}

impl SortMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alpha => "alpha",
            Self::Recent => "recent",
        }
    }
}

/// A saved sidebar group.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedSection {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    /// Finite; dense integers when projected from registers.
    pub order: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SidebarGroups {
    pub sections: Vec<SavedSection>,
    pub assignments: BTreeMap<String, String>,
}

/// Decoded sidebar preferences.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SidebarPreferences {
    pub sections: Vec<SavedSection>,
    pub assignments: BTreeMap<String, String>,
    pub starred: Vec<String>,
    pub muted: Vec<String>,
    /// Group key (`starred`, `channels`, `forums`, `dms`, `section:<id>`)
    /// to mode. Absent means alphabetical.
    pub sort: BTreeMap<String, SortMode>,
}

impl SidebarPreferences {
    pub fn sort_mode(&self, key: &str) -> SortMode {
        self.sort.get(key).copied().unwrap_or_default()
    }

    pub fn groups(&self) -> SidebarGroups {
        SidebarGroups {
            sections: self.sections.clone(),
            assignments: self.assignments.clone(),
        }
    }
}

/// The `groups` value of the Me kit record (§5a.8). Reading and
/// writing the kit belongs to the templates feature; the sidebar only
/// needs this shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeGroups {
    pub groups: Vec<MeGroup>,
    pub assignments: BTreeMap<String, String>,
    /// Channels pulled out of the main sidebar.
    pub channels: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeGroup {
    pub id: String,
    pub name: String,
    pub default_template_id: String,
}

impl MeGroups {
    /// `me-preferences.ts` `project`: order is the list position.
    pub fn project(&self) -> SidebarGroups {
        SidebarGroups {
            sections: self
                .groups
                .iter()
                .enumerate()
                .map(|(order, group)| SavedSection {
                    id: group.id.clone(),
                    name: group.name.clone(),
                    icon: None,
                    order: order as f64,
                })
                .collect(),
            assignments: self.assignments.clone(),
        }
    }
}

/// Confirmed preferences saved at startup are shown, display only, when
/// at most a day old (`sidebar-preferences-store.ts:420-462`).
pub fn startup_cache_usable(saved_at_ms: u64, now_ms: u64) -> bool {
    saved_at_ms <= now_ms && now_ms - saved_at_ms <= 86_400_000
}

const MAX_SAFE: u64 = 9_007_199_254_740_991;
const LEGACY_DEVICE: &str = "0000000000000000";
const FIXED_GROUPS: [&str; 4] = ["starred", "channels", "forums", "dms"];
/// Plaintext budget of one record, and of retained register metadata.
pub const PLAINTEXT_LIMIT: usize = 128 * 1024;

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// Non-blank text of at most `max` UTF-16 units.
fn text_ok(value: &Value, max: usize) -> bool {
    value
        .as_str()
        .is_some_and(|s| !s.trim().is_empty() && utf16_len(s) <= max)
}

fn as_object<'a>(
    value: Option<&'a Value>,
    message: &'static str,
) -> Result<&'a Map<String, Value>, SidebarError> {
    match value {
        Some(Value::Object(map)) => Ok(map),
        _ => invalid(message),
    }
}

fn is_one(value: Option<&Value>) -> bool {
    value.and_then(Value::as_f64) == Some(1.0)
}

/// `Number.isSafeInteger`.
fn safe_int(value: &Value) -> Option<i64> {
    if let Some(i) = value.as_i64() {
        return (i.unsigned_abs() <= MAX_SAFE).then_some(i);
    }
    let f = value.as_f64()?;
    (f.fract() == 0.0 && f.abs() <= MAX_SAFE as f64).then_some(f as i64)
}

/// JavaScript `Math.round`: halves round up.
fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}

/// A JSON number as JavaScript would serialize it: integral values are
/// written without a fraction.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= MAX_SAFE as f64 {
        json!(value as i64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

/// Deep equality with JavaScript number semantics (`1` equals `1.0`).
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| json_eq(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

fn plaintext_len(value: &Map<String, Value>) -> usize {
    serde_json::to_string(value).map_or(usize::MAX, |s| s.len())
}

/// Sections and sort carry registers; stars and mutes do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterCoordinate {
    Sections,
    Sort,
}

impl RegisterCoordinate {
    fn of(coordinate: Coordinate) -> Option<Self> {
        match coordinate {
            Coordinate::Sections => Some(Self::Sections),
            Coordinate::Sort => Some(Self::Sort),
            _ => None,
        }
    }
}

/// `[version, device, value]`: a non-negative safe-integer millisecond
/// version, 16 lowercase hex device, and a value `valid` accepts.
fn register(value: &Value, valid: impl Fn(&Value) -> bool) -> Result<(), SidebarError> {
    let ok = value.as_array().is_some_and(|r| {
        r.len() == 3
            && safe_int(&r[0]).is_some_and(|v| v >= 0)
            && r[1].as_str().is_some_and(|d| {
                d.len() == 16 && d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            })
            && valid(&r[2])
    });
    if ok {
        Ok(())
    } else {
        invalid("Invalid sidebar register")
    }
}

fn register_map(
    value: Option<&Value>,
    max: usize,
    parse: impl Fn(&Value) -> Result<(), SidebarError>,
) -> Result<Map<String, Value>, SidebarError> {
    let map = match value {
        None => return Ok(Map::new()),
        Some(value) => as_object(Some(value), "Invalid sidebar metadata")?,
    };
    for (key, value) in map {
        if key == "__proto__" || key.trim().is_empty() || utf16_len(key) > max {
            return invalid("Invalid sidebar metadata key");
        }
        parse(value)?;
    }
    Ok(map.clone())
}

/// Validates register metadata and returns its tree (`sidebar-registers.ts`
/// `metadata`). `reading` tolerates any text on retained tombstones; the
/// live projection and every write are strict. Unknown keys fail closed.
fn metadata(
    coordinate: RegisterCoordinate,
    value: &Value,
    reading: bool,
) -> Result<Map<String, Value>, SidebarError> {
    let data = as_object(Some(value), "Invalid sidebar metadata")?;
    // Tombstones outlive the live projection caps, so retained nodes are
    // bounded by bytes rather than counts.
    if plaintext_len(data) > PLAINTEXT_LIMIT {
        return invalid("Sidebar metadata budget exceeded");
    }
    let allowed: &[&str] = match coordinate {
        RegisterCoordinate::Sections => &["v", "s", "a"],
        RegisterCoordinate::Sort => &["v", "g"],
    };
    if !is_one(data.get("v")) || data.keys().any(|key| !allowed.contains(&key.as_str())) {
        return invalid("Unsupported sidebar metadata");
    }
    let mut tree = Map::new();
    match coordinate {
        RegisterCoordinate::Sort => {
            let g = register_map(data.get("g"), 264, |v| {
                register(v, |v| {
                    v.is_null() || matches!(v.as_str(), Some("alpha" | "recent"))
                })
            })?;
            tree.insert("g".into(), Value::Object(g));
        }
        RegisterCoordinate::Sections => {
            let s = register_map(data.get("s"), 256, |node| {
                let fields = as_object(Some(node), "Invalid sidebar metadata")?;
                for (key, value) in fields {
                    match key.as_str() {
                        "name" if reading => register(value, Value::is_string),
                        "name" => register(value, |v| text_ok(v, 256)),
                        "icon" if reading => register(value, |v| v.is_null() || v.is_string()),
                        "icon" => register(value, |v| v.is_null() || text_ok(v, 128)),
                        "order" => register(value, |v| safe_int(v).is_some()),
                        "live" => register(value, Value::is_boolean),
                        _ => invalid("Invalid sidebar section register"),
                    }?;
                }
                Ok(())
            })?;
            let a = register_map(data.get("a"), 256, |v| {
                register(v, |v| v.is_null() || text_ok(v, 256))
            })?;
            tree.insert("s".into(), Value::Object(s));
            tree.insert("a".into(), Value::Object(a));
        }
    }
    Ok(tree)
}

fn reg_value<'a>(node: &'a Value, key: &str) -> Option<&'a Value> {
    node.get(key)
        .and_then(Value::as_array)
        .and_then(|r| r.get(2))
}

/// The legacy fields a register tree stands for (`sidebar-registers.ts`
/// `projection`): live named sections ordered by `(order, id)` and
/// renumbered densely, assignments to live sections, non-null sort modes.
fn projection(coordinate: RegisterCoordinate, tree: &Map<String, Value>) -> Map<String, Value> {
    let entries = |key: &str| {
        tree.get(key)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
    };
    let mut out = Map::new();
    match coordinate {
        RegisterCoordinate::Sort => {
            let groups: Map<String, Value> = entries("g")
                .filter_map(|(key, reg)| {
                    reg.get(2)
                        .filter(|v| !v.is_null())
                        .map(|v| (key.clone(), v.clone()))
                })
                .collect();
            out.insert("groups".into(), Value::Object(groups));
        }
        RegisterCoordinate::Sections => {
            let mut sections: Vec<(String, Value, f64, Option<Value>)> = entries("s")
                .filter(|(_, node)| {
                    reg_value(node, "live") == Some(&Value::Bool(true))
                        && reg_value(node, "name").is_some_and(Value::is_string)
                })
                .map(|(id, node)| {
                    let order = reg_value(node, "order")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    let icon = reg_value(node, "icon")
                        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()));
                    (
                        id.clone(),
                        reg_value(node, "name").cloned().unwrap_or_default(),
                        order,
                        icon.cloned(),
                    )
                })
                .collect();
            sections.sort_by(|a, b| {
                a.2.partial_cmp(&b.2)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| a.0.encode_utf16().cmp(b.0.encode_utf16()))
            });
            let live: HashSet<&str> = sections.iter().map(|s| s.0.as_str()).collect();
            let assignments: Map<String, Value> = entries("a")
                .filter_map(|(key, reg)| {
                    reg.get(2)
                        .and_then(Value::as_str)
                        .filter(|id| live.contains(id))
                        .map(|id| (key.clone(), json!(id)))
                })
                .collect();
            let sections: Vec<Value> = sections
                .iter()
                .enumerate()
                .map(|(order, (id, name, _, icon))| {
                    let mut section = object(json!({"id": id, "name": name, "order": order}));
                    if let Some(icon) = icon {
                        section.insert("icon".into(), icon.clone());
                    }
                    Value::Object(section)
                })
                .collect();
            out.insert("sections".into(), Value::Array(sections));
            out.insert("assignments".into(), Value::Object(assignments));
        }
    }
    out
}

/// Metadata is authoritative: replaces the legacy fields with the register
/// projection (`projectSidebarRecord`). Records without `meta` are returned
/// unchanged.
pub fn project_record(
    coordinate: RegisterCoordinate,
    value: &Map<String, Value>,
) -> Result<Map<String, Value>, SidebarError> {
    let Some(meta) = value.get("meta") else {
        return Ok(value.clone());
    };
    let mut out = value.clone();
    out.extend(projection(coordinate, &metadata(coordinate, meta, true)?));
    Ok(out)
}

/// Turns a meta-less head into registers stamped at the event time in
/// milliseconds, with the all-zero legacy device.
fn import_legacy(
    coordinate: RegisterCoordinate,
    value: &Map<String, Value>,
    created_at: u64,
) -> Result<Map<String, Value>, SidebarError> {
    let stamp = |v: Value| json!([created_at.saturating_mul(1000), LEGACY_DEVICE, v]);
    let mut tree = Map::new();
    match coordinate {
        RegisterCoordinate::Sort => {
            let groups: Map<String, Value> =
                as_object(value.get("groups"), "Invalid sidebar preferences")?
                    .iter()
                    .filter(|(_, v)| matches!(v.as_str(), Some("alpha" | "recent")))
                    .map(|(key, v)| (key.clone(), stamp(v.clone())))
                    .collect();
            tree.insert("g".into(), Value::Object(groups));
        }
        RegisterCoordinate::Sections => {
            let mut s = Map::new();
            for section in value
                .get("sections")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let (Some(id), Some(name), Some(order)) = (
                    section.get("id").and_then(Value::as_str),
                    section.get("name"),
                    section.get("order").and_then(Value::as_f64),
                ) else {
                    return invalid("Invalid sidebar preferences");
                };
                s.insert(
                    id.to_owned(),
                    json!({
                        "name": stamp(name.clone()),
                        "icon": stamp(section.get("icon").cloned().unwrap_or(Value::Null)),
                        "order": stamp(number(js_round(order))),
                        "live": stamp(json!(true)),
                    }),
                );
            }
            let a: Map<String, Value> =
                as_object(value.get("assignments"), "Invalid sidebar preferences")?
                    .iter()
                    .filter(|(_, v)| v.as_str().is_some_and(|id| s.contains_key(id)))
                    .map(|(key, v)| (key.clone(), stamp(v.clone())))
                    .collect();
            tree.insert("s".into(), Value::Object(s));
            tree.insert("a".into(), Value::Object(a));
        }
    }
    Ok(tree)
}

fn max_version(tree: &Map<String, Value>) -> u64 {
    tree.values()
        .map(|value| match value {
            Value::Array(r) => r.first().and_then(Value::as_u64).unwrap_or(0),
            Value::Object(node) => max_version(node),
            _ => 0,
        })
        .max()
        .unwrap_or(0)
}

/// This process's register device: 8 random bytes in hex, in memory only.
pub fn process_device() -> &'static str {
    static DEVICE: OnceLock<String> = OnceLock::new();
    DEVICE.get_or_init(|| {
        SecretKey::generate().to_secret_bytes()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    })
}

/// One register write: a path like `["a", channelId]` or
/// `["s", sectionId, "live"]`, and the new value.
pub type RegisterWrite = (Vec<String>, Value);

/// Writes only the requested leaves, preserving every other register and
/// tombstone (`editSidebarRecord`). Returns `None` when every write already
/// holds; removing an assignment that is not projected is already
/// satisfied. The new version is `max(now_ms, newest + 1)`.
pub fn edit_record(
    coordinate: RegisterCoordinate,
    current: &Map<String, Value>,
    created_at: u64,
    writes: &[RegisterWrite],
    now_ms: u64,
    device: &str,
) -> Result<Option<Map<String, Value>>, SidebarError> {
    let mut tree = match current.get("meta") {
        None => import_legacy(coordinate, current, created_at)?,
        Some(meta) => metadata(coordinate, meta, true)?,
    };
    let projected = projection(coordinate, &tree);
    let changed: Vec<&RegisterWrite> = writes
        .iter()
        .filter(|(path, value)| {
            let mut node = Some(&tree);
            let mut leaf = None;
            for key in path {
                leaf = node.and_then(|n| n.get(key));
                node = leaf.and_then(Value::as_object);
            }
            if value.is_null()
                && coordinate == RegisterCoordinate::Sections
                && path.len() == 2
                && path[0] == "a"
                && !projected
                    .get("assignments")
                    .and_then(Value::as_object)
                    .is_some_and(|a| a.contains_key(&path[1]))
            {
                return false;
            }
            !leaf
                .and_then(Value::as_array)
                .is_some_and(|r| r.get(2).is_some_and(|v| json_eq(v, value)))
        })
        .collect();
    if changed.is_empty() {
        return Ok(None);
    }
    let version = now_ms.max(max_version(&tree).saturating_add(1));
    if version > MAX_SAFE {
        return invalid("Sidebar register clock exhausted");
    }
    for (path, value) in changed {
        let Some((last, parents)) = path.split_last() else {
            return invalid("Invalid sidebar register path");
        };
        let mut node = &mut tree;
        for key in parents {
            let child = node
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            node = match child {
                Value::Object(map) => map,
                _ => return invalid("Invalid sidebar register path"),
            };
        }
        node.insert(last.clone(), json!([version, device, value]));
    }
    let mut meta = Map::new();
    meta.insert("v".into(), json!(1));
    meta.extend(tree);
    let meta = Value::Object(meta);
    let projected = projection(coordinate, &metadata(coordinate, &meta, false)?);
    let mut next = current.clone();
    next.extend(projected);
    next.insert("meta".into(), meta);
    Ok(Some(next))
}

/// The order a new section is appended at: one past the largest live
/// canonical order, not the dense projection.
pub fn next_section_order(current: &Map<String, Value>) -> Result<f64, SidebarError> {
    let Some(meta) = current.get("meta") else {
        let max = current
            .get("sections")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|s| s.get("order").and_then(Value::as_f64))
            .map(js_round)
            .fold(-1.0, f64::max);
        return Ok(max + 1.0);
    };
    let tree = metadata(RegisterCoordinate::Sections, meta, false)?;
    let max = tree
        .get("s")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, node)| reg_value(node, "live") == Some(&Value::Bool(true)))
        .map(|(_, node)| {
            reg_value(node, "order")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        })
        .fold(-1.0, f64::max);
    Ok(max + 1.0)
}

fn non_blank(value: &Value, max: usize) -> Result<String, SidebarError> {
    if text_ok(value, max) {
        Ok(value.as_str().unwrap_or_default().to_owned())
    } else {
        invalid("Invalid sidebar preference text")
    }
}

fn key_text(key: &str, max: usize) -> Result<(), SidebarError> {
    non_blank(&json!(key), max).map(|_| ())
}

fn project_sections(value: &Value, into: &mut SidebarPreferences) -> Result<(), SidebarError> {
    let data = project_record(
        RegisterCoordinate::Sections,
        as_object(Some(value), "Invalid sidebar preferences")?,
    )?;
    let sections = match data.get("sections") {
        Some(Value::Array(sections)) if is_one(data.get("version")) && sections.len() <= 100 => {
            sections
        }
        _ => return invalid("Unsupported sidebar groups"),
    };
    let mut ids = HashSet::new();
    let mut parsed = Vec::with_capacity(sections.len());
    for raw in sections {
        let entry = as_object(Some(raw), "Invalid sidebar preferences")?;
        let id = non_blank(entry.get("id").unwrap_or(&Value::Null), 256)?;
        let order = entry.get("order").and_then(Value::as_f64);
        let Some(order) = order.filter(|_| ids.insert(id.clone())) else {
            return invalid("Invalid sidebar group order");
        };
        let name = non_blank(entry.get("name").unwrap_or(&Value::Null), 256)?;
        let icon = entry
            .get("icon")
            .map(|icon| non_blank(icon, 128))
            .transpose()?;
        parsed.push(SavedSection {
            id,
            name,
            icon,
            order,
        });
    }
    parsed.sort_by(|a, b| {
        a.order
            .partial_cmp(&b.order)
            .unwrap_or(Ordering::Equal)
            .then_with(|| locale_compare(&a.id, &b.id))
    });
    let assignments = as_object(data.get("assignments"), "Invalid sidebar preferences")?;
    if assignments.len() > 1000 {
        return invalid("Sidebar assignment budget exceeded");
    }
    let mut kept = BTreeMap::new();
    for (channel, section) in assignments {
        key_text(channel, 256)?;
        let section = non_blank(section, 256)?;
        if ids.contains(&section) {
            kept.insert(channel.clone(), section);
        }
    }
    into.sections = parsed;
    into.assignments = kept;
    Ok(())
}

fn project_toggles(
    value: &Value,
    field: &str,
    unsupported: &'static str,
    budget: &'static str,
    entry_error: &'static str,
) -> Result<Vec<String>, SidebarError> {
    let data = as_object(Some(value), "Invalid sidebar preferences")?;
    if !is_one(data.get("version")) {
        return invalid(unsupported);
    }
    let channels = as_object(data.get("channels"), "Invalid sidebar preferences")?;
    if channels.len() > 500 {
        return invalid(budget);
    }
    let mut on = Vec::new();
    for (id, raw) in channels {
        key_text(id, 256)?;
        let entry = as_object(Some(raw), "Invalid sidebar preferences")?;
        let flag = entry.get(field).and_then(Value::as_bool);
        let updated = entry.get("updatedAt").and_then(Value::as_f64);
        let (Some(flag), Some(updated)) = (flag, updated) else {
            return invalid(entry_error);
        };
        if updated < 0.0 {
            return invalid(entry_error);
        }
        if flag {
            on.push(id.clone());
        }
    }
    Ok(on)
}

fn project_sort(
    value: &Value,
    section_ids: Option<&[String]>,
) -> Result<BTreeMap<String, SortMode>, SidebarError> {
    let data = project_record(
        RegisterCoordinate::Sort,
        as_object(Some(value), "Invalid sidebar preferences")?,
    )?;
    if !is_one(data.get("version")) {
        return invalid("Unsupported sidebar sort");
    }
    let groups = as_object(data.get("groups"), "Invalid sidebar preferences")?;
    if groups.len() > 104 {
        return invalid("Sidebar sort budget exceeded");
    }
    let live: Option<HashSet<String>> =
        section_ids.map(|ids| ids.iter().map(|id| format!("section:{id}")).collect());
    let mut sort = BTreeMap::new();
    for (key, mode) in groups {
        let mode = match mode.as_str() {
            Some("alpha") => SortMode::Alpha,
            Some("recent") => SortMode::Recent,
            _ => continue,
        };
        key_text(key, 264)?;
        let known = FIXED_GROUPS.contains(&key.as_str())
            || match &live {
                Some(live) => live.contains(key),
                None => key.starts_with("section:") && key.len() > 8,
            };
        if known {
            sort.insert(key.clone(), mode);
        }
    }
    Ok(sort)
}

/// Projects decoded records into preferences (`projectSidebarPreferences`).
/// Each record is optional; malformed or unsupported records fail closed.
/// `sort_section_ids`, when given, limits `section:<id>` sort keys to those
/// sections.
pub fn project_preferences(
    sections: Option<&Value>,
    stars: Option<&Value>,
    mutes: Option<&Value>,
    sort: Option<&Value>,
    sort_section_ids: Option<&[String]>,
) -> Result<SidebarPreferences, SidebarError> {
    let mut result = SidebarPreferences::default();
    if let Some(sections) = sections {
        project_sections(sections, &mut result)?;
    }
    if let Some(stars) = stars {
        result.starred = project_toggles(
            stars,
            "starred",
            "Unsupported sidebar stars",
            "Sidebar star budget exceeded",
            "Invalid sidebar star",
        )?;
    }
    if let Some(sort) = sort {
        result.sort = project_sort(sort, sort_section_ids)?;
    }
    if let Some(mutes) = mutes {
        result.muted = project_toggles(
            mutes,
            "muted",
            "Unsupported channel mutes",
            "Channel mute budget exceeded",
            "Invalid channel mute",
        )?;
    }
    Ok(result)
}

fn project_one(
    coordinate: Coordinate,
    value: &Value,
    section_ids: Option<&[String]>,
) -> Result<SidebarPreferences, SidebarError> {
    let pick = |c: Coordinate| (coordinate == c).then_some(value);
    project_preferences(
        pick(Coordinate::Sections),
        pick(Coordinate::Stars),
        pick(Coordinate::Mutes),
        pick(Coordinate::Sort),
        section_ids,
    )
}

// ---------------------------------------------------------------------------
// Preferences: edits (`sidebar-edits.ts`)
// ---------------------------------------------------------------------------

/// Channel and section ids: non-blank, at most 256 UTF-16 units, never
/// normalized.
pub fn valid_sidebar_id(id: &str) -> bool {
    !id.trim().is_empty() && utf16_len(id) <= 256
}

/// Move a channel to a group, out of every group, or into a new group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentIntent {
    pub channel_id: String,
    pub section_id: Option<String>,
    pub create_section: Option<NewSection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSection {
    /// A client-chosen lowercase UUID, kept across retries.
    pub id: String,
    pub name: String,
}

pub fn valid_assignment(intent: &AssignmentIntent) -> bool {
    valid_sidebar_id(&intent.channel_id)
        && intent.section_id.as_deref().is_none_or(valid_sidebar_id)
        && intent.create_section.as_ref().is_none_or(|new| {
            intent.section_id.is_none() && is_uuid(&new.id) && valid_sidebar_id(&new.name)
        })
}

/// A sort may target a fixed group or a known live section.
pub fn valid_sort(group: &str, section_ids: &[String]) -> bool {
    utf16_len(group) <= 264
        && section_ids.len() <= 100
        && section_ids.iter().all(|id| valid_sidebar_id(id))
        && (FIXED_GROUPS.contains(&group)
            || group
                .strip_prefix("section:")
                .is_some_and(|id| section_ids.iter().any(|s| s == id)))
}

fn saved_sections(current: &Map<String, Value>) -> Vec<(String, String, f64)> {
    current
        .get("sections")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some((
                s.get("id")?.as_str()?.to_owned(),
                s.get("name")?.as_str()?.to_owned(),
                s.get("order").and_then(Value::as_f64).unwrap_or(0.0),
            ))
        })
        .collect()
}

/// Tombstones a section and clears its projected assignments in the same
/// record.
pub fn edit_section_removal(
    current: &Map<String, Value>,
    created_at: u64,
    section_id: &str,
    now_ms: u64,
    device: &str,
) -> Result<Option<Map<String, Value>>, SidebarError> {
    let projected = project_record(RegisterCoordinate::Sections, current)?;
    if !saved_sections(&projected)
        .iter()
        .any(|(id, _, _)| id == section_id)
    {
        return Ok(None);
    }
    let mut writes: Vec<RegisterWrite> = vec![(
        vec!["s".into(), section_id.into(), "live".into()],
        json!(false),
    )];
    for (channel, id) in projected
        .get("assignments")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if id.as_str() == Some(section_id) {
            writes.push((vec!["a".into(), channel.clone()], Value::Null));
        }
    }
    edit_record(
        RegisterCoordinate::Sections,
        current,
        created_at,
        &writes,
        now_ms,
        device,
    )
}

/// Assigns a channel; creating the section first when asked. Retrying a
/// creation with the same id but another name is refused.
pub fn edit_assignment(
    current: &Map<String, Value>,
    created_at: u64,
    intent: &AssignmentIntent,
    now_ms: u64,
    device: &str,
) -> Result<Option<Map<String, Value>>, SidebarError> {
    let section = intent
        .create_section
        .as_ref()
        .map(|new| new.id.clone())
        .or_else(|| intent.section_id.clone());
    let existing = saved_sections(current);
    let mut sections = existing.clone();
    if let Some(new) = &intent.create_section {
        let name = new.name.trim();
        match sections.iter().find(|(id, _, _)| *id == new.id) {
            Some((_, found, _)) if found != name => {
                return invalid("The new section changed; reload and try again");
            }
            Some(_) => {}
            None => sections.push((
                new.id.clone(),
                name.to_owned(),
                next_section_order(current)?,
            )),
        }
    }
    if let Some(section) = &section
        && !sections.iter().any(|(id, _, _)| id == section)
    {
        return invalid("Sidebar group no longer exists");
    }
    let mut writes: Vec<RegisterWrite> = vec![(
        vec!["a".into(), intent.channel_id.clone()],
        section.clone().map_or(Value::Null, Value::String),
    )];
    if let Some(section) = &section
        && intent.create_section.is_some()
        && !existing.iter().any(|(id, _, _)| id == section)
    {
        let Some((id, name, order)) = sections.iter().find(|(id, _, _)| id == section) else {
            return invalid("Sidebar group no longer exists");
        };
        let path = |field: &str| vec!["s".to_owned(), id.clone(), field.to_owned()];
        writes.push((path("name"), json!(name)));
        writes.push((path("icon"), Value::Null));
        writes.push((path("order"), number(*order)));
        writes.push((path("live"), json!(true)));
    }
    edit_record(
        RegisterCoordinate::Sections,
        current,
        created_at,
        &writes,
        now_ms,
        device,
    )
}

/// Alphabetical is stored as `null` (the default).
pub fn edit_sort(
    current: &Map<String, Value>,
    created_at: u64,
    group: &str,
    mode: SortMode,
    now_ms: u64,
    device: &str,
) -> Result<Option<Map<String, Value>>, SidebarError> {
    let value = match mode {
        SortMode::Alpha => Value::Null,
        SortMode::Recent => json!("recent"),
    };
    edit_record(
        RegisterCoordinate::Sort,
        current,
        created_at,
        &[(vec!["g".into(), group.into()], value)],
        now_ms,
        device,
    )
}

/// Star or mute toggle. Unstarring an absent entry is a no-op; unmuting
/// always persists an explicit `muted: false` tombstone.
pub fn edit_toggle(
    current: &Map<String, Value>,
    channel_id: &str,
    field: ToggleField,
    enabled: bool,
    now_ms: u64,
) -> Result<Option<Map<String, Value>>, SidebarError> {
    let key = field.key();
    let channels = as_object(current.get("channels"), "Invalid sidebar preferences")?;
    let previous = channels.get(channel_id).and_then(Value::as_object);
    if previous.and_then(|p| p.get(key)).and_then(Value::as_bool) == Some(enabled)
        || (previous.is_none() && !enabled && field == ToggleField::Starred)
    {
        return Ok(None);
    }
    let mut entry = previous.cloned().unwrap_or_default();
    let updated = previous
        .and_then(|p| p.get("updatedAt"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    entry.insert(key.into(), json!(enabled));
    entry.insert(
        "updatedAt".into(),
        number((now_ms as f64).max(updated + 1.0)),
    );
    let mut channels = channels.clone();
    channels.insert(channel_id.to_owned(), Value::Object(entry));
    let mut next = current.clone();
    next.insert("channels".into(), Value::Object(channels));
    Ok(Some(next))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToggleField {
    Starred,
    Muted,
}

impl ToggleField {
    fn key(self) -> &'static str {
        match self {
            Self::Starred => "starred",
            Self::Muted => "muted",
        }
    }
}

// ---------------------------------------------------------------------------
// Preferences: wire codec (`identity.rs:338-750`, `native-sidebar.ts`)
// ---------------------------------------------------------------------------

/// Most records one decode accepts, and their total serialized size.
pub const DECODE_MAX_EVENTS: usize = 4;
pub const DECODE_MAX_BYTES: usize = 768 * 1024;

/// The startup read: one `limit: 1` filter per coordinate, foreground
/// priority, wrapped in a 10 s timeout by the session.
pub fn preference_filters(viewer: &str) -> Vec<Filter> {
    Coordinate::ALL
        .into_iter()
        .map(|c| object(json!({"kinds": [kinds::APP_DATA], "authors": [viewer], "#d": [c.as_str()], "limit": 1})))
        .collect()
}

/// The strong head read before a write and for its readback.
pub fn head_filter(viewer: &str, coordinate: Coordinate) -> Filter {
    object(json!({
        "kinds": [kinds::APP_DATA],
        "authors": [viewer],
        "#d": [coordinate.as_str()],
        "limit": 1,
        "consistency": "strong",
    }))
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedRecord {
    /// The decrypted JSON, not yet validated.
    pub payload: Value,
    pub created_at: u64,
}

/// Decrypted records by coordinate.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecodedRecords(pub BTreeMap<Coordinate, DecodedRecord>);

impl DecodedRecords {
    pub fn get(&self, coordinate: Coordinate) -> Option<&DecodedRecord> {
        self.0.get(&coordinate)
    }

    /// The preferences these records describe.
    pub fn preferences(&self) -> Result<SidebarPreferences, SidebarError> {
        let payload = |c| self.get(c).map(|r| &r.payload);
        project_preferences(
            payload(Coordinate::Sections),
            payload(Coordinate::Stars),
            payload(Coordinate::Mutes),
            payload(Coordinate::Sort),
            None,
        )
    }
}

/// Decrypts verified sidebar records. Each must be our own kind 30078 with
/// exactly one `d` naming a known coordinate, at most once per batch
/// (`identity.rs:635-688`). Anything else fails the whole batch.
pub fn decode_records(
    identity: &Identity,
    events: &[Verified],
) -> Result<DecodedRecords, SidebarError> {
    let total: usize = events.iter().map(Verified::byte_size).sum::<usize>() + events.len() + 1;
    if events.len() > DECODE_MAX_EVENTS || total > DECODE_MAX_BYTES {
        return invalid("Invalid sidebar records");
    }
    let mut decoded = BTreeMap::new();
    for event in events {
        let d: Vec<&[String]> = event.tags_named("d").collect();
        let coordinate = match d.as_slice() {
            [only] => only.get(1).and_then(|c| Coordinate::parse(c)),
            _ => None,
        };
        let Some(coordinate) = coordinate.filter(|c| {
            event.kind_u16() == kinds::APP_DATA
                && event.pubkey == identity.public_key()
                && !decoded.contains_key(c)
        }) else {
            return invalid("Invalid sidebar record");
        };
        let plaintext = identity
            .decrypt_from_self(event)
            .map_err(|_| SidebarError::Invalid("Invalid sidebar record"))?;
        if plaintext.len() > PLAINTEXT_LIMIT {
            return invalid("Sidebar plaintext budget exceeded");
        }
        let payload = serde_json::from_str(&plaintext)
            .map_err(|_| SidebarError::Invalid("Invalid sidebar record"))?;
        decoded.insert(
            coordinate,
            DecodedRecord {
                payload,
                created_at: event.created_at_secs(),
            },
        );
    }
    Ok(DecodedRecords(decoded))
}

/// Checks a record before it is encrypted and signed (`identity.rs`
/// `validate_sidebar_payload`): the payload schema and limits, strict
/// register metadata, and legacy fields equal to the metadata projection.
pub fn validate_payload(
    coordinate: Coordinate,
    value: &Map<String, Value>,
) -> Result<(), SidebarError> {
    const BAD: &str = "Invalid sidebar payload";
    if value.get("version").and_then(Value::as_u64) != Some(1) {
        return invalid(BAD);
    }
    let key_ok = |key: &str, max: usize| !key.trim().is_empty() && utf16_len(key) <= max;
    match coordinate {
        Coordinate::Sections => {
            let (Some(sections), Some(assignments)) = (
                value.get("sections").and_then(Value::as_array),
                value.get("assignments").and_then(Value::as_object),
            ) else {
                return invalid(BAD);
            };
            if sections.len() > 100 || assignments.len() > 1000 {
                return invalid(BAD);
            }
            let mut ids = HashSet::new();
            for section in sections {
                let Some(entry) = section.as_object() else {
                    return invalid(BAD);
                };
                let id = entry.get("id").unwrap_or(&Value::Null);
                if !text_ok(id, 256)
                    || !ids.insert(id.as_str().unwrap_or_default())
                    || !entry.get("name").is_some_and(|v| text_ok(v, 256))
                    || !entry.get("order").is_some_and(Value::is_number)
                    || entry.get("icon").is_some_and(|v| !text_ok(v, 128))
                {
                    return invalid(BAD);
                }
            }
            if assignments
                .iter()
                .any(|(id, section)| !key_ok(id, 256) || !text_ok(section, 256))
            {
                return invalid(BAD);
            }
        }
        Coordinate::Stars | Coordinate::Mutes => {
            let Some(channels) = value.get("channels").and_then(Value::as_object) else {
                return invalid(BAD);
            };
            let field = if coordinate == Coordinate::Stars {
                "starred"
            } else {
                "muted"
            };
            if channels.len() > 500
                || channels.iter().any(|(id, entry)| {
                    !key_ok(id, 256)
                        || !entry.get(field).is_some_and(Value::is_boolean)
                        || !entry
                            .get("updatedAt")
                            .and_then(Value::as_f64)
                            .is_some_and(|v| v >= 0.0)
                })
            {
                return invalid(BAD);
            }
        }
        Coordinate::Sort => {
            let Some(groups) = value.get("groups").and_then(Value::as_object) else {
                return invalid(BAD);
            };
            if groups.len() > 104
                || groups
                    .iter()
                    .any(|(group, mode)| utf16_len(group) > 264 || !mode.is_string())
            {
                return invalid(BAD);
            }
        }
    }
    if let (Some(meta), Some(register)) = (value.get("meta"), RegisterCoordinate::of(coordinate)) {
        let tree = metadata(register, meta, false)
            .map_err(|_| SidebarError::Invalid("Invalid sidebar metadata"))?;
        let projected = projection(register, &tree);
        if projected
            .iter()
            .any(|(key, v)| !value.get(key).is_some_and(|data| json_eq(data, v)))
        {
            return invalid("Sidebar projection disagrees with metadata");
        }
    }
    Ok(())
}

/// `created_at` for a new head: never before or equal to the head it
/// replaces.
pub fn next_created_at(now_secs: u64, head_created_at: u64) -> u64 {
    now_secs.max(head_created_at + 1)
}

/// Builds the unsigned, self-encrypted record for `payload`, tagged exactly
/// `[["d",c],["t",c]]` (`identity.rs:708-746`). Sign it with
/// [`Identity::sign`] and [`Purpose::AppData`].
pub fn record_template(
    identity: &Identity,
    coordinate: Coordinate,
    payload: &Map<String, Value>,
    created_at: u64,
) -> Result<UnsignedEvent, SidebarError> {
    validate_payload(coordinate, payload)?;
    let plaintext = serde_json::to_string(payload)
        .map_err(|_| SidebarError::Invalid("Invalid sidebar payload"))?;
    if plaintext.len() > PLAINTEXT_LIMIT {
        return invalid("Sidebar plaintext budget exceeded");
    }
    let content = identity
        .encrypt_to_self(&plaintext)
        .map_err(|_| SidebarError::Invalid("Could not encrypt sidebar preferences"))?;
    let c = coordinate.as_str().to_owned();
    Ok(identity.template(
        Purpose::AppData,
        kinds::APP_DATA,
        created_at,
        vec![vec!["d".into(), c.clone()], vec!["t".into(), c]],
        content,
    )?)
}

/// Admits a record for publication only if it is shaped exactly like
/// [`record_template`] output (`identity.rs` `admit_sidebar`).
pub fn admit_record(identity: &Identity, event: &Verified) -> Result<Coordinate, SidebarError> {
    let decoded = decode_records(identity, std::slice::from_ref(event))?;
    let Some((coordinate, record)) = decoded.0.iter().next() else {
        return invalid("Invalid sidebar record");
    };
    let c = coordinate.as_str();
    let tags: Vec<&[String]> = event.tags.iter().map(|t| t.as_slice()).collect();
    if tags
        != [
            &["d".to_owned(), c.to_owned()][..],
            &["t".to_owned(), c.to_owned()][..],
        ]
    {
        return invalid("Invalid sidebar record");
    }
    let Value::Object(payload) = &record.payload else {
        return invalid("Invalid sidebar payload");
    };
    validate_payload(*coordinate, payload)?;
    Ok(*coordinate)
}

// ---------------------------------------------------------------------------
// Preferences: writes (`native-sidebar.ts`)
// ---------------------------------------------------------------------------

/// The newest record of one coordinate, as a write starts from it.
#[derive(Clone, Debug, PartialEq)]
pub struct SidebarHead {
    /// Validated, with sections and sort projected from their metadata.
    pub payload: Map<String, Value>,
    /// 0 when no record exists.
    pub created_at: u64,
}

impl SidebarHead {
    pub fn empty(coordinate: Coordinate) -> Self {
        Self {
            payload: coordinate.empty(),
            created_at: 0,
        }
    }

    /// Builds the head from a strong [`head_filter`] read. An unreadable or
    /// unsupported head fails closed; it is never rewritten.
    pub fn from_decoded(
        coordinate: Coordinate,
        decoded: &DecodedRecords,
    ) -> Result<Self, SidebarError> {
        let Some(record) = decoded.get(coordinate) else {
            return Ok(Self::empty(coordinate));
        };
        let Value::Object(value) = &record.payload else {
            return invalid("Invalid sidebar record");
        };
        project_one(coordinate, &record.payload, None)?;
        let payload = match RegisterCoordinate::of(coordinate) {
            Some(register) => project_record(register, value)?,
            None => value.clone(),
        };
        Ok(Self {
            payload,
            created_at: record.created_at,
        })
    }
}

/// One sidebar preference write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarWrite {
    Assign(AssignmentIntent),
    RemoveSection {
        section_id: String,
    },
    Star {
        channel_id: String,
        starred: bool,
    },
    Mute {
        channel_id: String,
        muted: bool,
    },
    Sort {
        group: String,
        mode: SortMode,
        section_ids: Vec<String>,
    },
}

/// What a write reports once confirmed.
#[derive(Clone, Debug, PartialEq)]
pub enum WriteResult {
    Groups(SidebarGroups),
    Starred(Vec<String>),
    Muted(Vec<String>),
    Sort(BTreeMap<String, SortMode>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedWrite {
    /// The record to sign and publish, or `None` when the head already
    /// shows the intent (nothing is published).
    pub next: Option<Map<String, Value>>,
    pub result: WriteResult,
}

impl SidebarWrite {
    pub fn coordinate(&self) -> Coordinate {
        match self {
            Self::Assign(_) | Self::RemoveSection { .. } => Coordinate::Sections,
            Self::Star { .. } => Coordinate::Stars,
            Self::Mute { .. } => Coordinate::Mutes,
            Self::Sort { .. } => Coordinate::Sort,
        }
    }

    /// Refuses malformed intents before any read or signature.
    pub fn validate(&self) -> Result<(), SidebarError> {
        let (ok, message) = match self {
            Self::Assign(intent) => (
                valid_assignment(intent),
                "Invalid sidebar assignment intent",
            ),
            Self::RemoveSection { section_id } => (
                valid_sidebar_id(section_id),
                "Invalid sidebar section removal intent",
            ),
            Self::Star { channel_id, .. } => {
                (valid_sidebar_id(channel_id), "Invalid sidebar star intent")
            }
            Self::Mute { channel_id, .. } => {
                (valid_sidebar_id(channel_id), "Invalid sidebar mute intent")
            }
            Self::Sort {
                group, section_ids, ..
            } => (
                valid_sort(group, section_ids),
                "Invalid sidebar sort intent",
            ),
        };
        if ok { Ok(()) } else { invalid(message) }
    }

    /// The error when the readback does not show the intent.
    pub fn conflict(&self) -> SidebarError {
        SidebarError::Conflict(match self.coordinate() {
            Coordinate::Sections => {
                "Sidebar groups changed on another device; reload and try again"
            }
            Coordinate::Stars => "Sidebar stars changed on another device; reload and try again",
            Coordinate::Mutes => "Sidebar mutes changed on another device; reload and try again",
            Coordinate::Sort => "Sidebar sort changed on another device; reload and try again",
        })
    }

    /// Applies the intent to a head. Unchanged registers and tombstones are
    /// preserved; new registers are stamped with [`process_device`].
    pub fn prepare(&self, head: &SidebarHead, now_ms: u64) -> Result<PreparedWrite, SidebarError> {
        let current = &head.payload;
        let device = process_device();
        let next = match self {
            Self::Assign(intent) => {
                edit_assignment(current, head.created_at, intent, now_ms, device)?
            }
            Self::RemoveSection { section_id } => {
                edit_section_removal(current, head.created_at, section_id, now_ms, device)?
            }
            Self::Star {
                channel_id,
                starred,
            } => edit_toggle(current, channel_id, ToggleField::Starred, *starred, now_ms)?,
            Self::Mute { channel_id, muted } => {
                edit_toggle(current, channel_id, ToggleField::Muted, *muted, now_ms)?
            }
            Self::Sort { group, mode, .. } => {
                edit_sort(current, head.created_at, group, *mode, now_ms, device)?
            }
        };
        let next = next.filter(|next| {
            !json_eq(
                &Value::Object(next.clone()),
                &Value::Object(current.clone()),
            )
        });
        let record = next.as_ref().unwrap_or(current);
        let result = self.result(record)?;
        if let Some(next) = &next
            && plaintext_len(next) > PLAINTEXT_LIMIT
        {
            return invalid("Sidebar plaintext budget exceeded");
        }
        Ok(PreparedWrite { next, result })
    }

    fn result(&self, record: &Map<String, Value>) -> Result<WriteResult, SidebarError> {
        let value = Value::Object(record.clone());
        Ok(match self {
            Self::Assign(_) | Self::RemoveSection { .. } => {
                WriteResult::Groups(project_one(Coordinate::Sections, &value, None)?.groups())
            }
            Self::Star { .. } => {
                WriteResult::Starred(project_one(Coordinate::Stars, &value, None)?.starred)
            }
            Self::Mute { .. } => {
                WriteResult::Muted(project_one(Coordinate::Mutes, &value, None)?.muted)
            }
            Self::Sort { section_ids, .. } => {
                WriteResult::Sort(project_one(Coordinate::Sort, &value, Some(section_ids))?.sort)
            }
        })
    }

    /// Does the strong readback show the requested intent? If not, report
    /// [`Self::conflict`]; if so, the confirmed result is
    /// `prepare(readback)?.result`.
    pub fn confirms(&self, readback: &SidebarHead) -> Result<bool, SidebarError> {
        let value = &readback.payload;
        Ok(match self {
            Self::RemoveSection { section_id } => {
                let groups =
                    project_one(Coordinate::Sections, &Value::Object(value.clone()), None)?;
                !groups.sections.iter().any(|s| s.id == *section_id)
                    && !groups.assignments.values().any(|id| id == section_id)
            }
            Self::Assign(intent) => {
                let section = intent
                    .create_section
                    .as_ref()
                    .map(|new| new.id.as_str())
                    .or(intent.section_id.as_deref());
                let assigned = value
                    .get("assignments")
                    .and_then(|a| a.get(&intent.channel_id))
                    .and_then(Value::as_str);
                assigned == section
                    && intent.create_section.as_ref().is_none_or(|new| {
                        saved_sections(value).iter().any(|(id, name, _)| {
                            Some(id.as_str()) == section && name == new.name.trim()
                        })
                    })
            }
            Self::Sort { group, mode, .. } => {
                let saved = value
                    .get("groups")
                    .and_then(|g| g.get(group))
                    .and_then(Value::as_str);
                saved.unwrap_or("alpha") == mode.as_str()
            }
            Self::Star {
                channel_id,
                starred: enabled,
            }
            | Self::Mute {
                channel_id,
                muted: enabled,
            } => {
                let field = if matches!(self, Self::Star { .. }) {
                    "starred"
                } else {
                    "muted"
                };
                let entry = value.get("channels").and_then(|c| c.get(channel_id));
                entry.and_then(|e| e.get(field)).and_then(Value::as_bool) == Some(*enabled)
                    || (field == "starred" && !*enabled && entry.is_none())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Verifier;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};

    fn row(id: &str) -> ChannelSummary {
        ChannelSummary {
            id: id.into(),
            name: id.into(),
            ..Default::default()
        }
    }

    fn with(id: &str, edit: impl FnOnce(&mut ChannelSummary)) -> ChannelSummary {
        let mut channel = row(id);
        edit(&mut channel);
        channel
    }

    fn dm(id: &str) -> ChannelSummary {
        with(id, |c| c.channel_type = Some(ChannelType::Dm))
    }

    fn section(id: &str, name: &str, order: f64) -> SavedSection {
        SavedSection {
            id: id.into(),
            name: name.into(),
            icon: None,
            order,
        }
    }

    fn prefs(
        sections: Vec<SavedSection>,
        assignments: &[(&str, &str)],
        starred: &[&str],
    ) -> SidebarPreferences {
        SidebarPreferences {
            sections,
            assignments: assignments
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            starred: starred.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn layout(sections: &[SidebarSection<'_>]) -> Vec<(String, Vec<String>)> {
        sections
            .iter()
            .map(|s| (s.key.clone(), s.rows.iter().map(|r| r.id.clone()).collect()))
            .collect()
    }

    fn pairs(items: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        items
            .iter()
            .map(|(k, ids)| (k.to_string(), ids.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    fn hidden(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn map(value: Value) -> Map<String, Value> {
        object(value)
    }

    #[test]
    fn identifies_section_keys() {
        assert!(is_channel_section_key("channels"));
        assert!(is_channel_section_key("group:engineering"));
        for key in ["starred", "forums", "dms"] {
            assert!(!is_channel_section_key(key));
        }
        for key in ["starred", "channels", "group:work"] {
            assert!(is_channel_drop_section_key(key));
        }
        for key in ["forums", "dms"] {
            assert!(!is_channel_drop_section_key(key));
        }
    }

    #[test]
    fn starred_shows_only_for_an_authorized_starred_row() {
        assert!(
            !sidebar_sections(&[], None, &hidden(&[]))
                .iter()
                .any(|s| s.key == "starred")
        );
        let p = prefs(vec![], &[], &["alpha"]);
        assert!(
            !sidebar_sections(&[], Some(&p), &hidden(&[]))
                .iter()
                .any(|s| s.key == "starred")
        );
        let channels = [row("alpha")];
        let sections = sidebar_sections(&channels, Some(&p), &hidden(&[]));
        assert_eq!(sections[0].key, "starred");
        assert_eq!(sections[0].icon.as_deref(), Some("★"));
        assert_eq!(sections[0].rows, vec![&channels[0]]);
    }

    #[test]
    fn intersects_groups_and_stars_with_active_streams_and_movable_dms() {
        let roster = vec![
            row("star"),
            row("work"),
            row("other"),
            with("archived", |c| c.archived = true),
            with("hidden", |c| c.hidden = true),
            dm("home-dm"),
            with("grouped-dm", |c| {
                c.channel_type = Some(ChannelType::Dm);
                c.participants = Some(vec!["a".into(), "b".into()]);
            }),
            dm("starred-dm"),
            dm("hidden-dm"),
            with("forum", |c| c.channel_type = Some(ChannelType::Forum)),
            with("session", |c| c.channel_type = Some(ChannelType::Session)),
        ];
        let mut p = prefs(
            vec![SavedSection {
                icon: Some(":party:".into()),
                ..section("channels", "Channels", 0.0)
            }],
            &[
                ("star", "channels"),
                ("work", "channels"),
                ("revoked", "channels"),
                ("grouped-dm", "channels"),
                ("starred-dm", "channels"),
                ("hidden-dm", "channels"),
                ("other", "missing"),
            ],
            &[
                "star",
                "archived",
                "hidden",
                "revoked",
                "starred-dm",
                "hidden-dm",
                "forum",
                "session",
            ],
        );
        let expected = pairs(&[
            ("starred", &["session", "star", "starred-dm"]),
            ("group:channels", &["grouped-dm", "work"]),
            ("channels", &["other"]),
            ("forums", &["forum"]),
            ("dms", &["home-dm"]),
        ]);
        let sections = sidebar_sections(&roster, Some(&p), &hidden(&["hidden-dm"]));
        assert_eq!(layout(&sections), expected);
        assert_eq!(sections[1].icon.as_deref(), Some(":party:"));
        // Every visible DM is placed exactly once.
        for id in ["home-dm", "grouped-dm", "starred-dm"] {
            let placements = sections
                .iter()
                .flat_map(|s| &s.rows)
                .filter(|r| r.id == id)
                .count();
            assert_eq!(placements, 1, "{id}");
        }
        // Hidden conversations stay absent even with stale placements.
        let after = sidebar_sections(&roster, Some(&p), &hidden(&["starred-dm", "hidden-dm"]));
        let visible: Vec<&str> = after
            .iter()
            .flat_map(|s| &s.rows)
            .map(|r| r.id.as_str())
            .collect();
        assert!(!visible.contains(&"starred-dm") && !visible.contains(&"hidden-dm"));
        let without: Vec<ChannelSummary> = roster
            .iter()
            .filter(|c| c.id != "starred-dm")
            .cloned()
            .collect();
        assert_eq!(
            layout(&sidebar_sections(
                &without,
                Some(&p),
                &hidden(&["hidden-dm"])
            ))[0],
            ("starred".into(), vec!["session".into(), "star".into()])
        );
        assert_eq!(
            layout(&sidebar_sections(&[], None, &hidden(&[]))),
            pairs(&[("channels", &[]), ("dms", &[])])
        );
        let all: Vec<String> = sidebar_sections(&roster, None, &hidden(&["hidden-dm"]))
            .iter()
            .flat_map(|s| s.rows.iter().map(|r| r.id.clone()))
            .collect();
        assert_eq!(
            all,
            [
                "other",
                "session",
                "star",
                "work",
                "forum",
                "grouped-dm",
                "home-dm",
                "starred-dm"
            ]
        );
        p.sort.insert("channels".into(), SortMode::Recent);
        assert!(needs_activity(&p));
    }

    #[test]
    fn sorts_sections_independently_with_deterministic_fallbacks() {
        let named = |id: &str, name: &str, at: Option<u64>| {
            with(id, |c| {
                c.name = name.into();
                c.last_activity_at = at;
            })
        };
        let forum = |id: &str, name: &str, at: u64| {
            with(id, |c| {
                c.name = name.into();
                c.last_activity_at = Some(at);
                c.channel_type = Some(ChannelType::Forum);
            })
        };
        let roster = vec![
            named("z-id", "same", Some(20)),
            named("a-id", "Same", Some(20)),
            named("new", "Zulu", Some(30)),
            named("quiet-b", "beta", None),
            named("quiet-a", "Alpha", None),
            forum("forum-old", "Forum old", 5),
            forum("forum-new", "Forum new", 10),
        ];
        let mut p = prefs(
            vec![section("work", "Work", 0.0)],
            &[
                ("z-id", "work"),
                ("a-id", "work"),
                ("new", "work"),
                ("quiet-b", "work"),
                ("quiet-a", "work"),
            ],
            &[],
        );
        p.sort.insert("section:work".into(), SortMode::Recent);
        p.sort.insert("forums".into(), SortMode::Alpha);
        assert_eq!(
            layout(&sidebar_sections(&roster, Some(&p), &hidden(&[]))),
            pairs(&[
                ("group:work", &["new", "a-id", "z-id", "quiet-a", "quiet-b"]),
                ("channels", &[]),
                ("forums", &["forum-new", "forum-old"]),
                ("dms", &[]),
            ])
        );
        let plain = sidebar_sections(&roster, None, &hidden(&[]));
        let ids: Vec<&str> = plain[0].rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["quiet-a", "quiet-b", "a-id", "z-id", "new"]);
    }

    #[test]
    fn star_projection_is_exclusive_and_keeps_empty_groups() {
        let channels = [row("alpha"), row("beta")];
        let placements = |starred: &[&str]| {
            let p = prefs(
                vec![section("work", "Work", 0.0)],
                &[("beta", "work")],
                starred,
            );
            layout(&sidebar_sections(&channels, Some(&p), &hidden(&[])))
        };
        assert_eq!(
            placements(&["alpha", "beta"]),
            pairs(&[
                ("starred", &["alpha", "beta"]),
                ("group:work", &[]),
                ("channels", &[]),
                ("dms", &[])
            ])
        );
        assert_eq!(
            placements(&["alpha"]),
            pairs(&[
                ("starred", &["alpha"]),
                ("group:work", &["beta"]),
                ("channels", &[]),
                ("dms", &[])
            ])
        );
    }

    #[test]
    fn recent_affects_only_its_own_section() {
        let keys = ["starred", "section:work", "channels", "forums", "dms"];
        for key in keys {
            let mut channels = Vec::new();
            for (i, section) in keys.iter().enumerate() {
                for (prefix, name, at) in [("a", "Alpha", 10), ("z", "Zulu", 20)] {
                    channels.push(with(&format!("{prefix}-{i}"), |c| {
                        c.name = name.into();
                        c.last_activity_at = Some(at);
                        c.channel_type = match *section {
                            "forums" => Some(ChannelType::Forum),
                            "dms" => Some(ChannelType::Dm),
                            _ => None,
                        };
                    }));
                }
            }
            let mut p = prefs(
                vec![section("work", "Work", 0.0)],
                &[("a-1", "work"), ("z-1", "work")],
                &["a-0", "z-0"],
            );
            p.sort.insert(key.into(), SortMode::Recent);
            let names: Vec<Vec<&str>> = sidebar_sections(&channels, Some(&p), &hidden(&[]))
                .iter()
                .map(|s| s.rows.iter().map(|r| r.name.as_str()).collect())
                .collect();
            let want: Vec<Vec<&str>> = keys
                .iter()
                .map(|k| {
                    if *k == key {
                        vec!["Zulu", "Alpha"]
                    } else {
                        vec!["Alpha", "Zulu"]
                    }
                })
                .collect();
            assert_eq!(names, want, "{key}");
        }
    }

    #[test]
    fn labels_dms_by_participant_names() {
        let alice = "a".repeat(64);
        let bob = "b".repeat(64);
        let roster = vec![
            with("pair", |c| {
                c.channel_type = Some(ChannelType::Dm);
                c.hidden = true;
                c.participants = Some(vec![alice.clone(), bob.clone()]);
            }),
            with("self", |c| {
                c.channel_type = Some(ChannelType::Dm);
                c.participants = Some(vec![]);
            }),
            with("old", |c| c.archived = true),
            with("secret", |c| c.hidden = true),
            row("general"),
        ];
        let labelled = label_channels(&roster, |id, _| (id == alice).then(|| "Alice".to_owned()));
        let names: Vec<&str> = labelled.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Alice, bbbbbbbbbb", "Notes to self", "general"]);
        assert_eq!(label_profile_ids(&roster), [alice, bob]);
    }

    #[test]
    fn me_channels_leave_the_main_sidebar() {
        let me = MeGroups {
            groups: vec![MeGroup {
                id: "g".into(),
                name: "Mine".into(),
                default_template_id: String::new(),
            }],
            assignments: [("b".to_owned(), "g".to_owned())].into(),
            channels: vec!["a".into()],
        };
        let rest = without_me_channels(&[row("a"), row("b")], &me);
        assert_eq!(rest, vec![row("b")]);
    }

    #[test]
    fn hidden_dms_unhide_on_new_evidence() {
        let head = |id: &str, at: u64| MessageHead {
            id: id.into(),
            created_at: at,
        };
        let mut local = LocalHiddenDms::default();
        local.hide("a", Some(head("m1", 10)));
        local.hide("b", None);
        local.hide("c", Some(head("m5", 10)));
        let relay = hidden(&["r"]);
        assert_eq!(
            hidden_dm_union(&relay, &local),
            hidden(&["a", "b", "c", "r"])
        );

        // Same head, or an older one, keeps the DM hidden; a newer head
        // (or an equal time with a smaller id) shows it.
        assert!(!local.check_heads(|id| (id == "a").then(|| head("m1", 10))));
        assert!(!local.check_heads(|id| (id == "a").then(|| head("m0", 9))));
        assert!(local.check_heads(|id| match id {
            "a" => Some(head("m2", 11)),
            "c" => Some(head("m4", 10)),
            _ => None,
        }));
        assert_eq!(local.ids().collect::<Vec<_>>(), ["b"]);

        // First probe records the window; a later unknown id shows it.
        let generation = local.generation_of("b").unwrap();
        assert_eq!(local.probe_filter("b").unwrap()["limit"], json!(100));
        let ids = vec!["x".to_owned()];
        assert_eq!(
            local.apply_probe("b", generation, &ids, None),
            ProbeOutcome::Recorded
        );
        assert_eq!(local.entries()[0].baseline, Baseline::Empty);
        assert_eq!(local.probe_filter("b").unwrap()["limit"], json!(50));
        assert_eq!(
            local.apply_probe("b", generation, &ids, None),
            ProbeOutcome::Unchanged
        );
        let saved = local.to_json();
        assert_eq!(
            saved,
            json!([{"id": "b", "baseline": null, "knownIds": ["x"]}])
        );
        let mut restored = LocalHiddenDms::restore(&saved);
        let generation = restored.generation_of("b").unwrap();
        assert_eq!(
            restored.apply_probe("b", generation, &["x".into(), "y".into()], None),
            ProbeOutcome::Shown
        );
        assert!(!restored.contains("b"));

        // A send finishing after a re-hide does not undo the new hide.
        let mut local = LocalHiddenDms::default();
        local.hide("d", None);
        let at_send = local.generation_of("d").unwrap();
        local.hide("d", None);
        assert!(!local.sent("d", at_send));
        let current = local.generation_of("d").unwrap();
        assert!(local.sent("d", current));
        assert_eq!(hidden_dm_probe_backoff_ms(2), 2000);
        assert_eq!(
            LocalHiddenDms::restore(&json!([{"no": "id"}, 3]))
                .entries()
                .len(),
            0
        );
    }

    fn signed(keys: &Keys, kind: u16, at: u64, tags: Vec<Vec<&str>>) -> Verified {
        let event = EventBuilder::new(Kind::from_u16(kind), "x")
            .tags(tags.into_iter().map(|t| Tag::parse(t).unwrap()))
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(keys)
            .unwrap();
        Verifier::default().verify(event).unwrap()
    }

    #[test]
    fn activity_filters_and_merges() {
        let ids: Vec<String> = vec!["a".into(), "b".into()];
        let filters = activity_filters(&ids).unwrap();
        assert_eq!(
            Value::Object(filters[0].clone()),
            json!({"kinds": [9, 40002, 40008, 45001, 45003], "#h": ["a"], "limit": 1})
        );
        assert!(activity_filters(&[]).is_err());
        assert!(activity_filters(&["bad id".into()]).is_err());
        assert!(activity_filters(&(0..129).map(|i| i.to_string()).collect::<Vec<_>>()).is_err());

        let keys = Keys::generate();
        let mut activity = ChannelActivity::default();
        assert!(activity.accept(&[signed(&keys, 9, 10, vec![vec!["h", "a"]])]));
        assert!(!activity.accept(&[signed(&keys, 9, 5, vec![vec!["h", "a"]])]));
        assert!(!activity.accept(&[signed(&keys, 7, 50, vec![vec!["h", "a"]])]));
        assert!(!activity.accept(&[signed(&keys, 9, 50, vec![vec!["h", "a"], vec!["h", "b"]])]));
        assert_eq!(activity.last("a"), Some(10));

        // A refresh replaces values, but keeps a live advance made during it.
        activity.accept(&[signed(&keys, 9, 3, vec![vec!["h", "c"]])]);
        let mut refresh = activity.begin_refresh();
        activity.accept(&[signed(&keys, 9, 20, vec![vec!["h", "a"]])]);
        let ids: Vec<String> = vec!["a".into(), "b".into()];
        refresh.collect(
            &ids,
            &[
                signed(&keys, 9, 15, vec![vec!["h", "a"]]),
                signed(&keys, 45001, 7, vec![vec!["h", "b"]]),
                signed(&keys, 9, 99, vec![vec!["h", "z"]]),
            ],
        );
        activity.commit(&ids, refresh);
        assert_eq!(activity.last("a"), Some(20));
        assert_eq!(activity.last("b"), Some(7));
        assert_eq!(activity.last("c"), None);
        let mut channels = vec![row("a"), row("q")];
        activity.annotate(&mut channels);
        assert_eq!(channels[0].last_activity_at, Some(20));
        assert_eq!(channels[1].last_activity_at, None);

        // An authoritative empty answer clears an unchanged value.
        let refresh = activity.begin_refresh();
        activity.commit(&ids, refresh);
        assert_eq!(activity.last("a"), None);
    }

    // --- preferences projection -------------------------------------------

    fn groups_record() -> Value {
        json!({
            "version": 1,
            "sections": [{"id": "work", "name": "Work", "order": 0}],
            "assignments": {"general": "work", "orphan": "missing"},
        })
    }

    fn stars_record() -> Value {
        json!({
            "version": 1,
            "channels": {
                "general": {"starred": true, "updatedAt": 1},
                "removed": {"starred": false, "updatedAt": 2},
            },
        })
    }

    #[test]
    fn bounds_shape_strips_orphans_and_honors_unstar_tombstones() {
        let projected = project_preferences(
            Some(&groups_record()),
            Some(&stars_record()),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(projected.sections, vec![section("work", "Work", 0.0)]);
        assert_eq!(
            projected.assignments,
            [("general".to_owned(), "work".to_owned())].into()
        );
        assert_eq!(projected.starred, ["general"]);
        assert!(projected.muted.is_empty());

        let mut many = groups_record();
        many["sections"] = Value::Array(
            (0..101)
                .map(|i| json!({"id": i.to_string(), "name": "n", "order": i}))
                .collect(),
        );
        assert!(project_preferences(Some(&many), None, None, None, None).is_err());
        let mut long = groups_record();
        long["sections"] = json!([{"id": "x", "name": "n".repeat(257), "order": 0}]);
        assert_eq!(
            project_preferences(Some(&long), None, None, None, None),
            Err(SidebarError::Invalid("Invalid sidebar preference text"))
        );
        let negative = json!({"version": 1, "channels": {"x": {"starred": true, "updatedAt": -1}}});
        assert!(project_preferences(None, Some(&negative), None, None, None).is_err());
        let unsupported = json!({"version": 2, "channels": {}});
        assert_eq!(
            project_preferences(None, Some(&unsupported), None, None, None),
            Err(SidebarError::Invalid("Unsupported sidebar stars"))
        );
        let sort = json!({"version": 1, "groups": {"channels": "recent", "section:x": "recent", "section:": "recent", "bogus": "recent", "dms": "weird"}});
        let all = project_preferences(None, None, None, Some(&sort), None).unwrap();
        assert_eq!(
            all.sort,
            [
                ("channels".to_owned(), SortMode::Recent),
                ("section:x".to_owned(), SortMode::Recent)
            ]
            .into()
        );
        let limited = project_preferences(None, None, None, Some(&sort), Some(&[])).unwrap();
        assert_eq!(
            limited.sort,
            [("channels".to_owned(), SortMode::Recent)].into()
        );
    }

    const DEV: &str = "1234567890abcdef";

    fn reg(value: Value, version: u64) -> Value {
        json!([version, DEV, value])
    }

    fn sections_meta() -> Map<String, Value> {
        map(json!({
            "version": 1,
            "sections": [],
            "assignments": {},
            "meta": {
                "v": 1,
                "s": {
                    "live": {"name": reg(json!("Work"), 100), "order": reg(json!(99), 100), "live": reg(json!(true), 100), "icon": reg(Value::Null, 100)},
                    "dead": {"name": reg(json!("Deleted"), 100), "live": reg(json!(false), 100), "order": reg(json!(1000), 100)},
                },
                "a": {"a": reg(json!("live"), 100), "removed": reg(Value::Null, 100), "orphan": reg(json!("dead"), 100)},
            },
        }))
    }

    fn sections_of(record: &Map<String, Value>) -> SidebarPreferences {
        project_preferences(Some(&Value::Object(record.clone())), None, None, None, None).unwrap()
    }

    fn write(path: &[&str], value: Value) -> RegisterWrite {
        (path.iter().map(|s| s.to_string()).collect(), value)
    }

    #[test]
    fn metadata_wins_over_stale_legacy_fields_and_keeps_tombstones() {
        let current = sections_meta();
        let projected = sections_of(&current);
        assert_eq!(projected.sections, vec![section("live", "Work", 0.0)]);
        assert_eq!(
            projected.assignments,
            [("a".to_owned(), "live".to_owned())].into()
        );

        let next = edit_record(
            RegisterCoordinate::Sections,
            &current,
            1,
            &[write(&["a", "a"], Value::Null)],
            50,
            DEV,
        )
        .unwrap()
        .unwrap();
        assert!(sections_of(&next).assignments.is_empty());
        let mut expected = current["meta"].clone();
        expected["a"]["a"] = json!([101, DEV, null]);
        assert_eq!(next["meta"], expected);
        assert_eq!(current["meta"]["a"]["a"], reg(json!("live"), 100));
        assert_eq!(
            edit_record(
                RegisterCoordinate::Sections,
                &next,
                1,
                &[write(&["a", "a"], Value::Null)],
                50,
                DEV
            )
            .unwrap(),
            None
        );
        assert_eq!(next_section_order(&current).unwrap(), 100.0);
    }

    #[test]
    fn imports_a_metaless_head_at_event_milliseconds() {
        let current =
            map(json!({"version": 1, "groups": {"channels": "recent", "forums": "recent"}}));
        let next = edit_record(
            RegisterCoordinate::Sort,
            &current,
            10,
            &[write(&["g", "channels"], Value::Null)],
            1,
            DEV,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            Value::Object(next),
            json!({
                "version": 1,
                "groups": {"forums": "recent"},
                "meta": {"v": 1, "g": {
                    "channels": [10001, DEV, null],
                    "forums": [10000, "0000000000000000", "recent"],
                }},
            })
        );
    }

    #[test]
    fn tombstones_outlive_the_live_cap_but_live_values_do_not() {
        let tombstones: Map<String, Value> = (0..105)
            .map(|i| (format!("section:{i}"), reg(Value::Null, 100)))
            .collect();
        let value = map(json!({"version": 1, "groups": {}, "meta": {"v": 1, "g": tombstones}}));
        let next = edit_record(
            RegisterCoordinate::Sort,
            &value,
            1,
            &[write(&["g", "channels"], json!("recent"))],
            200,
            DEV,
        )
        .unwrap()
        .unwrap();
        let sort = project_preferences(None, None, None, Some(&Value::Object(next.clone())), None)
            .unwrap()
            .sort;
        assert_eq!(sort, [("channels".to_owned(), SortMode::Recent)].into());
        assert_eq!(next["meta"]["g"].as_object().unwrap().len(), 106);
        let live: Map<String, Value> = tombstones
            .keys()
            .map(|k| (k.clone(), reg(json!("recent"), 100)))
            .collect();
        let over = json!({"version": 1, "groups": {}, "meta": {"v": 1, "g": live}});
        assert_eq!(
            project_preferences(None, None, None, Some(&over), None),
            Err(SidebarError::Invalid("Sidebar sort budget exceeded"))
        );
    }

    #[test]
    fn dynamic_keys_are_plain_but_proto_is_refused() {
        let next = edit_record(
            RegisterCoordinate::Sections,
            &sections_meta(),
            1,
            &[write(&["a", "constructor"], json!("live"))],
            200,
            DEV,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            sections_of(&next).assignments,
            [
                ("a".to_owned(), "live".to_owned()),
                ("constructor".to_owned(), "live".to_owned())
            ]
            .into()
        );
        assert!(
            edit_record(
                RegisterCoordinate::Sections,
                &sections_meta(),
                1,
                &[write(&["a", "__proto__"], json!("live"))],
                200,
                DEV
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_metadata_fails_closed_on_reads_and_writes() {
        let max = MAX_SAFE + 1;
        for meta in [
            Value::Null,
            json!({"v": 2}),
            json!({"v": 1, "g": null}),
            json!({"v": 1, "other": {}}),
            json!({"v": 1, "g": {"channels": [1, DEV]}}),
            json!({"v": 1, "g": {"channels": [1, DEV, "no"]}}),
            json!({"v": 1, "g": {"channels": [1.1, DEV, "recent"]}}),
            json!({"v": 1, "g": {"channels": [-1, DEV, "recent"]}}),
            json!({"v": 1, "g": {"channels": [1, "bad", "recent"]}}),
            json!({"v": 1, "g": {"channels": [max, DEV, "recent"]}}),
        ] {
            let value = json!({"version": 1, "groups": {}, "meta": meta});
            assert!(
                project_preferences(None, None, None, Some(&value), None).is_err(),
                "{meta}"
            );
            let record = map(value);
            assert!(
                edit_record(
                    RegisterCoordinate::Sort,
                    &record,
                    1,
                    &[write(&["g", "channels"], json!("recent"))],
                    1,
                    DEV
                )
                .is_err(),
                "{meta}"
            );
        }
    }

    #[test]
    fn refuses_an_exhausted_register_clock() {
        let record = map(
            json!({"version": 1, "groups": {}, "meta": {"v": 1, "g": {"channels": reg(Value::Null, MAX_SAFE)}}}),
        );
        assert_eq!(
            edit_record(
                RegisterCoordinate::Sort,
                &record,
                1,
                &[write(&["g", "channels"], json!("recent"))],
                1,
                DEV
            ),
            Err(SidebarError::Invalid("Sidebar register clock exhausted"))
        );
    }

    #[test]
    fn oversized_tombstone_text_is_read_but_never_rewritten() {
        for field in ["name", "icon"] {
            let mut current = sections_meta();
            current["meta"]["s"]["live"][field] = reg(json!("x".repeat(257)), 100);
            assert_eq!(
                project_preferences(
                    Some(&Value::Object(current.clone())),
                    None,
                    None,
                    None,
                    None
                ),
                Err(SidebarError::Invalid("Invalid sidebar preference text")),
                "{field}"
            );
            current["meta"]["s"]["live"]["live"] = reg(json!(false), 100);
            assert!(sections_of(&current).sections.is_empty());
            assert_eq!(
                edit_record(
                    RegisterCoordinate::Sections,
                    &current,
                    1,
                    &[write(&["a", "a"], Value::Null)],
                    1,
                    DEV
                ),
                Ok(None)
            );
            assert_eq!(
                edit_record(
                    RegisterCoordinate::Sections,
                    &current,
                    1,
                    &[write(&["s", "dead", "icon"], Value::Null)],
                    1,
                    DEV
                ),
                Err(SidebarError::Invalid("Invalid sidebar register"))
            );
        }
    }

    #[test]
    fn malformed_envelopes_and_unknown_fields_are_not_salvaged() {
        for dead in [
            json!({"name": [1, "bad-device", "x".repeat(257)], "live": reg(json!(false), 100)}),
            json!({"name": reg(json!("Deleted"), 100), "live": reg(json!(false), 100), "future": reg(json!("keep"), 100)}),
        ] {
            let mut current = sections_meta();
            current["meta"]["s"]["dead"] = dead;
            assert!(
                project_preferences(
                    Some(&Value::Object(current.clone())),
                    None,
                    None,
                    None,
                    None
                )
                .is_err()
            );
            assert!(
                edit_record(
                    RegisterCoordinate::Sections,
                    &current,
                    1,
                    &[write(&["a", "a"], Value::Null)],
                    1,
                    DEV
                )
                .is_err()
            );
        }
    }

    #[test]
    fn missing_assignment_removal_is_a_noop_but_other_nulls_write() {
        let current = sections_meta();
        assert_eq!(
            edit_record(
                RegisterCoordinate::Sections,
                &current,
                1,
                &[write(&["a", "missing"], Value::Null)],
                1,
                DEV
            ),
            Ok(None)
        );
        let next = edit_record(
            RegisterCoordinate::Sections,
            &current,
            1,
            &[write(&["s", "dead", "icon"], Value::Null)],
            1,
            DEV,
        )
        .unwrap()
        .unwrap();
        assert_eq!(next["meta"]["s"]["dead"]["icon"][2], Value::Null);
        let sort = map(json!({"version": 1, "groups": {}, "meta": {"v": 1, "g": {}}}));
        let next = edit_record(
            RegisterCoordinate::Sort,
            &sort,
            1,
            &[write(&["g", "channels"], Value::Null)],
            1,
            DEV,
        )
        .unwrap()
        .unwrap();
        assert_eq!(next["meta"]["g"]["channels"][2], Value::Null);
    }

    #[test]
    fn validates_intents_without_normalizing_ids() {
        for id in ["work", " work ", &"x".repeat(256), &"😀".repeat(128)] {
            assert!(valid_sidebar_id(id), "{id}");
        }
        for id in ["", " ", &"x".repeat(257), &"😀".repeat(129)] {
            assert!(!valid_sidebar_id(id), "{id}");
        }
        let create = NewSection {
            id: "11111111-1111-1111-1111-111111111111".into(),
            name: " Work ".into(),
        };
        let intent = |section: Option<&str>, create: Option<NewSection>| AssignmentIntent {
            channel_id: "c".into(),
            section_id: section.map(str::to_owned),
            create_section: create,
        };
        assert!(valid_assignment(&intent(None, Some(create.clone()))));
        assert!(valid_assignment(&intent(None, None)));
        for bad in [
            intent(Some(""), None),
            intent(Some("existing"), Some(create.clone())),
            intent(
                None,
                Some(NewSection {
                    id: "bad".into(),
                    ..create.clone()
                }),
            ),
            intent(
                None,
                Some(NewSection {
                    name: " ".into(),
                    ..create.clone()
                }),
            ),
            intent(
                None,
                Some(NewSection {
                    name: "x".repeat(257),
                    ..create.clone()
                }),
            ),
        ] {
            assert!(!valid_assignment(&bad), "{bad:?}");
        }
        for group in FIXED_GROUPS {
            assert!(valid_sort(group, &[]));
        }
        let long = "x".repeat(256);
        assert!(valid_sort(
            &format!("section:{long}"),
            std::slice::from_ref(&long)
        ));
        assert!(!valid_sort("section:unknown", &[]));
        assert!(!valid_sort(
            &format!("section:{long}x"),
            &[format!("{long}x")]
        ));
        assert!(valid_sort("channels", &vec!["s".to_owned(); 100]));
        assert!(!valid_sort("channels", &vec!["s".to_owned(); 101]));
        assert!(!valid_sort("channels", &[" ".to_owned()]));
    }

    #[test]
    fn section_removal_tombstones_one_legacy_section() {
        let current = map(json!({
            "version": 1,
            "sections": [{"id": "work", "name": "Work", "order": 0}, {"id": "later", "name": "Later", "order": 1}],
            "assignments": {"alpha": "work", "beta": "later", "orphan": "missing"},
        }));
        let next = edit_section_removal(&current, 1_700_000_000, "work", 42, DEV)
            .unwrap()
            .unwrap();
        assert_eq!(next["meta"]["s"]["work"]["live"][2], json!(false));
        assert_eq!(next["meta"]["a"]["alpha"][2], Value::Null);
        assert_eq!(
            next["meta"]["a"]["beta"],
            json!([1_700_000_000_000u64, LEGACY_DEVICE, "later"])
        );
        let projected = sections_of(&next);
        assert_eq!(projected.sections, vec![section("later", "Later", 0.0)]);
        assert_eq!(
            projected.assignments,
            [("beta".to_owned(), "later".to_owned())].into()
        );
        assert_eq!(edit_section_removal(&next, 0, "work", 43, DEV), Ok(None));
        assert_eq!(
            edit_section_removal(&current, 0, "missing", 42, DEV),
            Ok(None)
        );
    }

    // --- codec and write flow ---------------------------------------------

    /// An in-memory relay holding the newest record per coordinate.
    struct Relay {
        identity: Identity,
        verifier: Verifier,
        records: BTreeMap<Coordinate, Verified>,
        signed: usize,
    }

    impl Relay {
        fn new() -> Self {
            Self {
                identity: Identity::ephemeral(),
                verifier: Verifier::default(),
                records: BTreeMap::new(),
                signed: 0,
            }
        }

        fn sign(
            &self,
            coordinate: Coordinate,
            payload: &Map<String, Value>,
            created_at: u64,
        ) -> Verified {
            let unsigned =
                record_template(&self.identity, coordinate, payload, created_at).unwrap();
            let event = self.identity.sign(Purpose::AppData, unsigned).unwrap();
            self.verifier.verify(event).unwrap()
        }

        fn store(&mut self, coordinate: Coordinate, value: Value, created_at: u64) {
            let event = self.sign_unchecked(coordinate, &value, created_at);
            self.records.insert(coordinate, event);
        }

        /// Bypasses payload validation, like a record from an older or
        /// newer client.
        fn sign_unchecked(
            &self,
            coordinate: Coordinate,
            value: &Value,
            created_at: u64,
        ) -> Verified {
            let content = self.identity.encrypt_to_self(&value.to_string()).unwrap();
            let c = coordinate.as_str().to_owned();
            let unsigned = self
                .identity
                .template(
                    Purpose::AppData,
                    30078,
                    created_at,
                    vec![vec!["d".into(), c.clone()], vec!["t".into(), c]],
                    content,
                )
                .unwrap();
            self.verifier
                .verify(self.identity.sign(Purpose::AppData, unsigned).unwrap())
                .unwrap()
        }

        fn head(&self, coordinate: Coordinate) -> Result<SidebarHead, SidebarError> {
            let events: Vec<Verified> =
                self.records.get(&coordinate).cloned().into_iter().collect();
            SidebarHead::from_decoded(coordinate, &decode_records(&self.identity, &events)?)
        }

        /// `native-sidebar.ts` `mutate`.
        fn write(&mut self, intent: &SidebarWrite) -> Result<WriteResult, SidebarError> {
            self.write_racing(intent, |_| {})
        }

        fn write_racing(
            &mut self,
            intent: &SidebarWrite,
            race: impl FnOnce(&mut Self),
        ) -> Result<WriteResult, SidebarError> {
            intent.validate()?;
            let coordinate = intent.coordinate();
            let head = self.head(coordinate)?;
            let prepared = intent.prepare(&head, 1_700_000_000_000)?;
            let Some(next) = prepared.next else {
                return Ok(prepared.result);
            };
            let event = self.sign(
                coordinate,
                &next,
                next_created_at(1_700_000_000, head.created_at),
            );
            self.signed += 1;
            admit_record(&self.identity, &event)?;
            self.records.insert(coordinate, event);
            race(self);
            let readback = self.head(coordinate)?;
            if !intent.confirms(&readback)? {
                return Err(intent.conflict());
            }
            Ok(intent.prepare(&readback, 1_700_000_000_000)?.result)
        }
    }

    #[test]
    fn decodes_four_self_encrypted_coordinates() {
        let mut relay = Relay::new();
        relay.store(Coordinate::Sections, groups_record(), 1);
        relay.store(Coordinate::Stars, stars_record(), 1);
        relay.store(
            Coordinate::Mutes,
            json!({"version": 1, "channels": {"general": {"muted": true, "updatedAt": 1}}}),
            1,
        );
        relay.store(
            Coordinate::Sort,
            json!({"version": 1, "groups": {"dms": "recent"}}),
            1,
        );
        let events: Vec<Verified> = relay.records.values().cloned().collect();
        let preferences = decode_records(&relay.identity, &events)
            .unwrap()
            .preferences()
            .unwrap();
        assert_eq!(
            preferences.assignments,
            [("general".to_owned(), "work".to_owned())].into()
        );
        assert_eq!(preferences.starred, ["general"]);
        assert_eq!(preferences.muted, ["general"]);
        assert_eq!(preferences.sort_mode("dms"), SortMode::Recent);

        let viewer = relay.identity.pubkey_hex();
        let filters = preference_filters(&viewer);
        assert_eq!(filters.len(), 4);
        assert_eq!(
            Value::Object(filters[1].clone()),
            json!({"kinds": [30078], "authors": [viewer], "#d": ["channel-stars"], "limit": 1})
        );
        assert_eq!(
            head_filter(&viewer, Coordinate::Sort)["consistency"],
            json!("strong")
        );
    }

    #[test]
    fn decode_fails_closed_on_foreign_or_ambiguous_records() {
        let relay = Relay::new();
        let sections = relay.sign_unchecked(Coordinate::Sections, &groups_record(), 1);
        // Duplicate coordinate.
        assert!(decode_records(&relay.identity, &[sections.clone(), sections.clone()]).is_err());
        // Another author.
        let other = Relay::new();
        let foreign = other.sign_unchecked(Coordinate::Sections, &groups_record(), 1);
        assert!(decode_records(&relay.identity, &[foreign]).is_err());
        // Two `d` tags, an unknown coordinate, and too many events.
        let keys_event = |tags: Vec<Vec<String>>| {
            let content = relay.identity.encrypt_to_self("{}").unwrap();
            let unsigned = relay
                .identity
                .template(Purpose::AppData, 30078, 1, tags, content)
                .unwrap();
            relay
                .verifier
                .verify(relay.identity.sign(Purpose::AppData, unsigned).unwrap())
                .unwrap()
        };
        let d = |v: &str| vec!["d".to_owned(), v.to_owned()];
        assert!(
            decode_records(
                &relay.identity,
                &[keys_event(vec![d("channel-sort"), d("channel-stars")])]
            )
            .is_err()
        );
        assert!(decode_records(&relay.identity, &[keys_event(vec![d("read-state:00")])]).is_err());
        let five: Vec<Verified> = (0..5).map(|_| sections.clone()).collect();
        assert!(decode_records(&relay.identity, &five).is_err());
        // A plaintext that is not JSON.
        let garbage = keys_event(vec![d("channel-sort")]);
        assert!(decode_records(&relay.identity, &[garbage]).is_ok());
        let content = relay.identity.encrypt_to_self("not json").unwrap();
        let unsigned = relay
            .identity
            .template(Purpose::AppData, 30078, 1, vec![d("channel-sort")], content)
            .unwrap();
        let bad = relay
            .verifier
            .verify(relay.identity.sign(Purpose::AppData, unsigned).unwrap())
            .unwrap();
        assert!(decode_records(&relay.identity, &[bad]).is_err());
    }

    #[test]
    fn creates_moves_stars_mutes_and_sorts_with_encrypted_readback() {
        let mut relay = Relay::new();
        let id = "12345678-1234-1234-1234-123456789abc";
        let assign = SidebarWrite::Assign(AssignmentIntent {
            channel_id: "c1".into(),
            section_id: None,
            create_section: Some(NewSection {
                id: id.into(),
                name: "Work".into(),
            }),
        });
        let WriteResult::Groups(groups) = relay.write(&assign).unwrap() else {
            panic!()
        };
        assert_eq!(
            groups.assignments,
            [("c1".to_owned(), id.to_owned())].into()
        );
        assert_eq!(groups.sections, vec![section(id, "Work", 0.0)]);
        let star = |starred| SidebarWrite::Star {
            channel_id: "c1".into(),
            starred,
        };
        let mute = |muted| SidebarWrite::Mute {
            channel_id: "c1".into(),
            muted,
        };
        let sort = |mode| SidebarWrite::Sort {
            group: format!("section:{id}"),
            mode,
            section_ids: vec![id.into()],
        };
        assert_eq!(
            relay.write(&star(true)).unwrap(),
            WriteResult::Starred(vec!["c1".into()])
        );
        assert_eq!(
            relay.write(&mute(true)).unwrap(),
            WriteResult::Muted(vec!["c1".into()])
        );
        assert_eq!(
            relay.write(&sort(SortMode::Recent)).unwrap(),
            WriteResult::Sort([(format!("section:{id}"), SortMode::Recent)].into())
        );
        assert_eq!(
            relay.write(&star(false)).unwrap(),
            WriteResult::Starred(vec![])
        );
        assert_eq!(
            relay.write(&mute(false)).unwrap(),
            WriteResult::Muted(vec![])
        );
        // The unstar tombstone is retained.
        let stars = relay.head(Coordinate::Stars).unwrap().payload;
        assert_eq!(stars["channels"]["c1"]["starred"], json!(false));
        assert_eq!(
            relay.write(&sort(SortMode::Alpha)).unwrap(),
            WriteResult::Sort(BTreeMap::new())
        );
        let unassign = SidebarWrite::Assign(AssignmentIntent {
            channel_id: "c1".into(),
            section_id: None,
            create_section: None,
        });
        let WriteResult::Groups(groups) = relay.write(&unassign).unwrap() else {
            panic!()
        };
        assert!(groups.assignments.is_empty());
        assert_eq!(relay.records.len(), 4);
        assert_eq!(relay.signed, 8);
        // Removing the section tombstones it.
        let remove = SidebarWrite::RemoveSection {
            section_id: id.into(),
        };
        let WriteResult::Groups(groups) = relay.write(&remove).unwrap() else {
            panic!()
        };
        assert!(groups.sections.is_empty());
        let meta = &relay.head(Coordinate::Sections).unwrap().payload["meta"];
        assert_eq!(meta["s"][id]["live"][2], json!(false));
        assert_eq!(meta["s"][id]["name"][2], json!("Work"));
    }

    #[test]
    fn a_competing_write_is_not_reported_as_saved() {
        let mut relay = Relay::new();
        let star = SidebarWrite::Star {
            channel_id: "c1".into(),
            starred: true,
        };
        let result = relay.write_racing(&star, |relay| {
            relay.store(
                Coordinate::Stars,
                json!({"version": 1, "channels": {}}),
                1_700_000_011,
            );
        });
        assert_eq!(
            result,
            Err(SidebarError::Conflict(
                "Sidebar stars changed on another device; reload and try again"
            ))
        );
    }

    #[test]
    fn unreadable_heads_and_redundant_intents_are_never_signed() {
        let mut relay = Relay::new();
        relay.store(Coordinate::Stars, json!({"version": 2, "channels": {}}), 1);
        let star = SidebarWrite::Star {
            channel_id: "c1".into(),
            starred: true,
        };
        assert_eq!(
            relay.write(&star),
            Err(SidebarError::Invalid("Unsupported sidebar stars"))
        );
        assert_eq!(relay.signed, 0);

        let mut relay = Relay::new();
        let unstar = SidebarWrite::Star {
            channel_id: "c1".into(),
            starred: false,
        };
        assert_eq!(relay.write(&unstar).unwrap(), WriteResult::Starred(vec![]));
        assert_eq!(relay.signed, 0);
        // Unmute of an absent entry still writes its tombstone.
        let unmute = SidebarWrite::Mute {
            channel_id: "c1".into(),
            muted: false,
        };
        relay.write(&unmute).unwrap();
        assert_eq!(relay.signed, 1);

        let bad = SidebarWrite::Sort {
            group: "section:nope".into(),
            mode: SortMode::Recent,
            section_ids: vec![],
        };
        assert_eq!(
            bad.validate(),
            Err(SidebarError::Invalid("Invalid sidebar sort intent"))
        );
    }

    #[test]
    fn writers_preserve_unrelated_registers_and_tombstones() {
        let mut relay = Relay::new();
        let stamp = |v: Value| json!([2_000_000_000_000u64, "1111111111111111", v]);
        relay.store(
            Coordinate::Sections,
            json!({
                "version": 1,
                "sections": [{"id": "work", "name": "Work", "order": 0}],
                "assignments": {"c1": "work"},
                "meta": {"v": 1,
                    "s": {"work": {"name": stamp(json!("Work")), "order": stamp(json!(40)), "live": stamp(json!(true))},
                          "deleted": {"name": stamp(json!("Deleted")), "live": stamp(json!(false))}},
                    "a": {"c1": stamp(json!("work")), "old": stamp(Value::Null)}},
            }),
            1,
        );
        let move_c2 = SidebarWrite::Assign(AssignmentIntent {
            channel_id: "c2".into(),
            section_id: Some("work".into()),
            create_section: None,
        });
        relay.write(&move_c2).unwrap();
        let meta = relay.head(Coordinate::Sections).unwrap().payload["meta"].clone();
        assert_eq!(
            meta["s"]["deleted"],
            json!({"name": stamp(json!("Deleted")), "live": stamp(json!(false))})
        );
        assert_eq!(meta["a"]["old"], stamp(Value::Null));
        assert_eq!(meta["a"]["c1"], stamp(json!("work")));
        // The register clock moves past the newest existing version.
        assert_eq!(meta["a"]["c2"][0], json!(2_000_000_000_001u64));
        assert_eq!(meta["a"]["c2"][1], json!(process_device()));

        // A new section is appended after the largest canonical order.
        let id = "abcdefab-1234-1234-1234-123456789abc";
        let create = SidebarWrite::Assign(AssignmentIntent {
            channel_id: "c3".into(),
            section_id: None,
            create_section: Some(NewSection {
                id: id.into(),
                name: "Next".into(),
            }),
        });
        relay.write(&create).unwrap();
        let head = relay.head(Coordinate::Sections).unwrap();
        assert_eq!(head.payload["meta"]["s"][id]["order"][2], json!(41));
        // Retrying the creation with another name is refused.
        let renamed = SidebarWrite::Assign(AssignmentIntent {
            channel_id: "c3".into(),
            section_id: None,
            create_section: Some(NewSection {
                id: id.into(),
                name: "Other".into(),
            }),
        });
        assert_eq!(
            relay.write(&renamed),
            Err(SidebarError::Invalid(
                "The new section changed; reload and try again"
            ))
        );
    }

    #[test]
    fn signer_refuses_payloads_that_disagree_with_metadata() {
        let identity = Identity::ephemeral();
        let mut payload = sections_meta();
        // The legacy fields here are stale relative to `meta`.
        assert_eq!(
            record_template(&identity, Coordinate::Sections, &payload, 1).unwrap_err(),
            SidebarError::Invalid("Sidebar projection disagrees with metadata")
        );
        payload.extend(projection(
            RegisterCoordinate::Sections,
            &metadata(RegisterCoordinate::Sections, &payload["meta"], false).unwrap(),
        ));
        let unsigned = record_template(&identity, Coordinate::Sections, &payload, 1).unwrap();
        let tags: Vec<Vec<String>> = unsigned
            .tags
            .iter()
            .map(|t| t.as_slice().to_vec())
            .collect();
        assert_eq!(tags, [["d", "channel-sections"], ["t", "channel-sections"]]);
        assert_eq!(unsigned.kind.as_u16(), 30078);
        assert!(
            record_template(
                &identity,
                Coordinate::Stars,
                &map(json!({"version": 1, "channels": {"x": {"starred": "yes", "updatedAt": 1}}})),
                1
            )
            .is_err()
        );
        assert_eq!(next_created_at(100, 100), 101);
        assert_eq!(next_created_at(100, 5), 100);
    }

    #[test]
    fn startup_cache_is_good_for_a_day() {
        assert!(startup_cache_usable(1_000, 1_000));
        assert!(startup_cache_usable(0, 86_400_000));
        assert!(!startup_cache_usable(0, 86_400_001));
        assert!(!startup_cache_usable(2_000, 1_000));
    }
}
