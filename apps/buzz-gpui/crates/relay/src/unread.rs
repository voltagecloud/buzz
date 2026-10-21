//! Unread computation from sampled, verified evidence.
//!
//! Ports buzz-app's `unread.ts`, `inbox.ts` and `thread-follows.ts`. Behavior
//! spec: `docs/port/relay.md` §5i.7–5i.10 and §5l.5.
//!
//! There is no server count API. Counts come from a bounded evidence store
//! (4096 events / 8 MiB) fed by every verified batch, plus one 500-row
//! sample per 128 channels at startup ([`Unread::begin_repair`]). They are
//! therefore "observed", not exact: a busy channel can fill its batch's
//! sample and starve the others (§9.3 #10; kept as in the reference).
//!
//! Like [`crate::read_state`], this module is synchronous. The session runs
//! the reads it asks for ([`EvidenceRepair`], [`LookupBatch`],
//! [`HomesRequest`]) and feeds results back. Mark actions build a
//! [`ReadUpdate`] and apply it to the [`ReadStateSync`] passed in.
//!
//! Mute (§9.3 #11): muting a channel changes no unread count, dot, badge or
//! read state. It only suppresses notifications, and only for direct
//! messages and thread replies, never mentions; see
//! [`MessageAttention::muted_suppresses`].

use crate::event::{Verified, is_hex64, kinds};
use crate::fold::{FoldOptions, has_tag};
use crate::read_state::{
    MarkCoverage, MessageRead, ReadCapability, ReadLookup, ReadMutationResult, ReadState,
    ReadStateError, ReadStateJournal, ReadStateSync, ReadTarget, ReadUpdate, effective_frontier,
    mark_message, override_active,
};
use crate::socket::{Filter, filter};
use nostr::prelude::{Event, PublicKey};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// Retained evidence bounds. Overflow clears all evidence.
pub const EVIDENCE_EVENTS: usize = 4096;
pub const EVIDENCE_BYTES: usize = 8 * 1024 * 1024;
/// The relay caps explicit `#h` values at 128 per request.
pub const REPAIR_BATCH: usize = 128;
/// Rows sampled per repair batch.
pub const REPAIR_LIMIT: usize = 500;
/// Explicit thread choices kept per partition; newest win.
pub const THREAD_FOLLOW_LIMIT: usize = 1000;
const LOOKUPS: usize = 4096;
const LOOKUP_EVENTS: usize = 1024;
const WITNESSES: usize = 4096;
const RETRACTED: usize = 1024;
const LOOKUP_CHUNK: usize = 50;
const LOOKUP_PAGES: u32 = 10;
const HOMES_BATCH: usize = 100;
const SNAPSHOT_CACHE: usize = 4096;
const READING_LEASES: usize = 64;
const VIEW_IDS: usize = 128;
const ANCESTRY_DEPTH: usize = 32;
/// Content kinds counted as messages.
pub const CONTENT_KINDS: [u16; 3] = [kinds::CHAT, kinds::AGENT_MESSAGE, kinds::DIFF];
const AUXILIARY_KINDS: [u16; 3] = [kinds::EDIT, kinds::DELETION, kinds::MODERATOR_DELETE];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnreadError {
    #[error(transparent)]
    ReadState(#[from] ReadStateError),
    #[error("Verified readable message evidence unavailable")]
    MessageUnavailable,
    #[error("Message does not belong to the read target")]
    NotInTarget,
    #[error("A thread reply cannot advance the channel frontier")]
    ReplyAdvancesChannel,
    #[error("Read target unavailable")]
    TargetUnavailable,
    #[error("Unread target unavailable")]
    UnreadTargetUnavailable,
    #[error("Unread channel visit expired")]
    VisitExpired,
    #[error("Thread unavailable")]
    ThreadUnavailable,
    #[error("Reading handle unavailable")]
    ReadingUnavailable,
    #[error("Could not save thread follows: {0}")]
    Follows(String),
}

type Result<T, E = UnreadError> = std::result::Result<T, E>;

// ---------------------------------------------------------------------------
// Event helpers
// ---------------------------------------------------------------------------

fn is_content(kind: u16) -> bool {
    CONTENT_KINDS.contains(&kind)
}

fn is_auxiliary(kind: u16) -> bool {
    AUXILIARY_KINDS.contains(&kind)
}

fn is_broadcast(event: &Event) -> bool {
    has_tag(event, "broadcast", "1")
}

fn e_targets(event: &Event) -> Vec<&str> {
    event
        .tags
        .iter()
        .filter(|tag| tag.kind() == "e")
        .filter_map(|tag| tag.as_slice().get(1).map(String::as_str))
        .filter(|id| !id.is_empty())
        .collect()
}

/// A NIP-10 reply's parent and root as lowercase hex, as evidence keys
/// them ([`crate::fold::thread_reference`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadReference {
    pub root_id: String,
    pub parent_id: String,
}

/// The last valid `root` and `reply` markers win; a lone root marker is not
/// a reply.
pub fn thread_reference(event: &Event) -> Option<ThreadReference> {
    crate::fold::thread_reference(event).map(|reference| ThreadReference {
        root_id: reference.root_id.to_hex(),
        parent_id: reference.parent_id.to_hex(),
    })
}

/// [`crate::fold::workflow_owner`] for a hex authority, as hex.
pub fn workflow_owner(event: &Event, authority: Option<&str>) -> Option<String> {
    let authority = authority.and_then(|key| PublicKey::from_hex(key).ok());
    crate::fold::workflow_owner(event, authority.as_ref()).map(|owner| owner.to_hex())
}

/// Preview text: agent envelopes (40002) carry `{"content": ...}`.
pub(crate) fn preview_text(event: &Event) -> String {
    if event.kind.as_u16() == kinds::AGENT_MESSAGE
        && let Ok(Value::Object(body)) = serde_json::from_str::<Value>(&event.content)
        && let Some(Value::String(content)) = body.get("content")
    {
        return content.clone();
    }
    event.content.clone()
}

fn timestamp(event: &Event) -> Result<u32> {
    u32::try_from(event.created_at.as_secs()).map_err(|_| ReadStateError::InvalidFrontier.into())
}

/// Sampled evidence reads: one per 128 channels, 500 rows each, with
/// auxiliary events (§5i.7 "Evidence repair").
pub fn evidence_filters(channel_ids: &[String]) -> Vec<Filter> {
    channel_ids
        .chunks(REPAIR_BATCH)
        .map(|batch| {
            filter(json!({
                "kinds": CONTENT_KINDS, "#h": batch, "include_aux": true, "limit": REPAIR_LIMIT,
            }))
        })
        .collect()
}

fn conversation_key(channel_id: &str, id: &str) -> String {
    format!("{channel_id}:{id}")
}

fn message_force_key(channel_id: &str) -> String {
    format!("message-force:{channel_id}")
}

/// Which channels own an event, through bounded reference-only ancestry.
/// A missing target, a cycle, or an unsupported intermediary fails closed.
fn owners(
    event: &Verified,
    find: &dyn Fn(&str) -> Option<Verified>,
    memo: &mut HashMap<String, Option<BTreeSet<String>>>,
    visiting: &mut HashSet<String>,
) -> Option<BTreeSet<String>> {
    let id = event.id_hex();
    if let Some(known) = memo.get(&id) {
        return known.clone();
    }
    if visiting.contains(&id) || visiting.len() >= ANCESTRY_DEPTH {
        return None;
    }
    visiting.insert(id.clone());
    let direct = event.channel();
    let kind = event.kind_u16();
    let resolved = if is_content(kind) {
        direct.map(|channel| BTreeSet::from([channel.to_owned()]))
    } else if is_auxiliary(kind) {
        let targets = e_targets(event);
        if direct.is_none() && targets.is_empty() {
            None
        } else {
            let mut set: BTreeSet<String> = direct.into_iter().map(str::to_owned).collect();
            let mut complete = true;
            for target in targets {
                match find(target).and_then(|target| owners(&target, find, memo, visiting)) {
                    Some(inherited) => set.extend(inherited),
                    None => {
                        complete = false;
                        break;
                    }
                }
            }
            (complete && !set.is_empty()).then_some(set)
        }
    } else {
        None
    };
    visiting.remove(&id);
    memo.insert(id, resolved.clone());
    resolved
}

/// An insertion-ordered map with oldest-first eviction (JS `Map` order).
#[derive(Clone, Debug)]
struct Lru<V> {
    map: HashMap<String, (u64, V)>,
    order: BTreeMap<u64, String>,
    next: u64,
}

impl<V> Default for Lru<V> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            order: BTreeMap::new(),
            next: 0,
        }
    }
}

impl<V> Lru<V> {
    fn get(&self, key: &str) -> Option<&V> {
        self.map.get(key).map(|(_, value)| value)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.map.get_mut(key).map(|(_, value)| value)
    }

    fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// Inserts as the newest entry, moving an existing key.
    fn insert(&mut self, key: String, value: V) {
        self.remove(&key);
        self.order.insert(self.next, key.clone());
        self.map.insert(key, (self.next, value));
        self.next += 1;
    }

    fn remove(&mut self, key: &str) -> Option<V> {
        let (seq, value) = self.map.remove(key)?;
        self.order.remove(&seq);
        Some(value)
    }

    fn pop_oldest(&mut self) -> Option<(String, V)> {
        let (_, key) = self.order.pop_first()?;
        let (_, value) = self.map.remove(&key)?;
        Some((key, value))
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    /// Oldest first.
    fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.order.values().map(|key| (key, &self.map[key].1))
    }

    fn retain(&mut self, mut keep: impl FnMut(&str, &V) -> bool) {
        let dropped: Vec<String> = self
            .iter()
            .filter(|(key, value)| !keep(key, value))
            .map(|(key, _)| key.clone())
            .collect();
        for key in dropped {
            self.remove(&key);
        }
    }
}

// ---------------------------------------------------------------------------
// Thread follows (`thread-follows.ts`, §5i.9)
// ---------------------------------------------------------------------------

/// Device-local explicit thread choices, keyed `<channel>:<root>`, oldest
/// first: `true` follows without replying, `false` unfollows even after
/// participating. Absent means automatic (participation).
pub trait ThreadFollowStore {
    fn read(&mut self) -> Vec<(String, bool)>;
    /// Fails when the choices were not saved.
    fn write(&mut self, choices: &[(String, bool)]) -> std::result::Result<(), String>;
}

#[derive(Debug, Default)]
pub struct MemoryThreadFollows {
    pub saved: Vec<(String, bool)>,
    pub fail_writes: bool,
}

impl ThreadFollowStore for MemoryThreadFollows {
    fn read(&mut self) -> Vec<(String, bool)> {
        self.saved.clone()
    }

    fn write(&mut self, choices: &[(String, bool)]) -> std::result::Result<(), String> {
        if self.fail_writes {
            return Err("storage unavailable".into());
        }
        self.saved = choices.to_vec();
        Ok(())
    }
}

/// Parses the saved JSON array of `[key, bool]`, ignoring malformed entries
/// and keeping the newest [`THREAD_FOLLOW_LIMIT`]. Unreadable text is empty.
pub fn parse_thread_follows(text: &str) -> Vec<(String, bool)> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let valid: Vec<(String, bool)> = items
        .iter()
        .filter_map(|item| match item.as_array()?.as_slice() {
            [Value::String(key), Value::Bool(choice), ..] => Some((key.clone(), *choice)),
            _ => None,
        })
        .collect();
    let start = valid.len().saturating_sub(THREAD_FOLLOW_LIMIT);
    // A repeated key keeps its first position and its last value (JS Map).
    let mut out: Vec<(String, bool)> = Vec::new();
    for (key, choice) in &valid[start..] {
        match out.iter_mut().find(|(existing, _)| existing == key) {
            Some(entry) => entry.1 = *choice,
            None => out.push((key.clone(), *choice)),
        }
    }
    out
}

pub fn encode_thread_follows(choices: &[(String, bool)]) -> String {
    serde_json::to_string(choices).expect("choices serialize")
}

// ---------------------------------------------------------------------------
// Public projections
// ---------------------------------------------------------------------------

/// A listed channel, as unread needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelInfo {
    pub id: String,
    /// `"dm"` for direct messages. `None` until metadata gives the type.
    pub channel_type: Option<String>,
    /// The viewer is in the channel's roster.
    pub member: bool,
    /// Listed from a display-only cached roster.
    pub cached: bool,
    pub archived: bool,
}

impl ChannelInfo {
    /// A confirmed channel the viewer belongs to.
    pub fn joined(id: impl Into<String>, channel_type: Option<&str>) -> Self {
        Self {
            id: id.into(),
            channel_type: channel_type.map(str::to_owned),
            member: true,
            cached: false,
            archived: false,
        }
    }

    fn allowed(&self) -> bool {
        !self.cached && self.member
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    Unknown,
    Observed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Unknown,
    Observed,
    Stale,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Manual {
    #[default]
    None,
    /// Marked unread on this device.
    LocalOnly,
    /// A peer's active override register.
    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestMessage {
    pub id: String,
    pub created_at: u64,
    /// The text of the newest message the channel's timeline shows (a
    /// top-level or broadcast one), edits applied, as a channel's
    /// last-message preview (`messagePreview`). Empty when there is none.
    pub preview: String,
}

/// A channel's last-message preview: the text, or for a diff its file and
/// description (`membership.ts` `messagePreview`).
fn latest_preview(event: &Event) -> String {
    if event.kind.as_u16() != kinds::DIFF {
        return crate::fold::displayed_text(event, &preview_text(event));
    }
    let tag = |name: &str| {
        event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .find(|tag| tag.len() > 1 && tag[0] == name)
            .map(|tag| tag[1].clone())
            .filter(|value| !value.is_empty())
    };
    let file = tag("file").unwrap_or_else(|| "patch".into());
    match tag("description") {
        Some(description) => format!("Diff: {file} — {description}"),
        None => format!("Diff: {file}"),
    }
}

/// Unread state of one target, for the UI. Counts are `None` when the
/// channel has no evidence yet or is not accessible: never a made-up zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnreadSnapshot {
    pub target: ReadTarget,
    /// Newest content evidence in the channel, read or not, including the
    /// viewer's own (channel targets only); ties go to the smaller ID.
    pub latest_message: Option<LatestMessage>,
    pub observed_count: Option<u32>,
    /// Unread rows that are mentions, direct, followed-thread replies or
    /// broadcasts.
    pub attention_count: Option<u32>,
    /// Unread rows that mention the viewer (a subset of attention). Not in
    /// the reference snapshot; derived with the same rules.
    pub mention_count: Option<u32>,
    pub coverage: Coverage,
    pub freshness: Freshness,
    pub manual: Manual,
    pub error: Option<String>,
}

/// How the sidebar draws a row (`UnreadBadge.tsx:50-64`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Badge {
    pub unread: bool,
    /// Ordinary unread: bold label.
    pub bold: bool,
    /// Attention: accent dot.
    pub priority: bool,
}

impl UnreadSnapshot {
    /// Unread for the dock dot and sidebar: manual intent or any row.
    pub fn unread(&self) -> bool {
        self.manual != Manual::None || self.observed_count.unwrap_or(0) > 0
    }

    pub fn badge(&self, dm: bool) -> Badge {
        let count = self.observed_count.unwrap_or(0);
        let attention = self.attention_count.unwrap_or(0);
        Badge {
            unread: self.unread(),
            bold: self.manual != Manual::None || if dm { count > 0 } else { count > attention },
            priority: dm || attention > 0,
        }
    }

    fn inaccessible(target: ReadTarget) -> Self {
        Self {
            target,
            latest_message: None,
            observed_count: None,
            attention_count: None,
            mention_count: None,
            coverage: Coverage::Unknown,
            freshness: Freshness::Unknown,
            manual: Manual::None,
            error: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionStatus {
    Unknown,
    Ineligible,
    Eligible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Mention,
    Direct,
    Thread,
}

/// One message's attention, with the same policy as the badges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageAttention {
    pub status: AttentionStatus,
    pub category: Option<Category>,
    /// Mentions the viewer (trusted workflow attribution excluded).
    pub mentioned: bool,
    pub root_id: Option<String>,
    /// A conversation lookup may still make this reply thread attention.
    pub pending: bool,
    pub unread: bool,
    /// Forced unread during the current channel visit.
    pub forced: bool,
    /// In a reading lease's visible rows.
    pub viewing: bool,
}

impl MessageAttention {
    fn unknown() -> Self {
        Self {
            status: AttentionStatus::Unknown,
            category: None,
            mentioned: false,
            root_id: None,
            pending: false,
            unread: false,
            forced: false,
            viewing: false,
        }
    }

    /// Whether muting the channel suppresses this message's notification:
    /// direct messages and thread replies, never mentions (§9.3 #11).
    pub fn muted_suppresses(&self) -> bool {
        !self.mentioned
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadActivityItem {
    pub channel_id: String,
    pub root_id: String,
    pub latest_message_id: String,
    pub author_id: String,
    pub workflow_owner_id: Option<String>,
    pub created_at: u64,
    /// Raw content (40002 unwrapped); edits are applied by the fold.
    pub preview: String,
    pub unread_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadActivitySnapshot {
    pub channel_id: String,
    /// `None` when evidence is unknown or access is denied.
    pub items: Option<Vec<ThreadActivityItem>>,
    pub coverage: Coverage,
    pub freshness: Freshness,
    pub error: Option<String>,
}

/// One explicit prefix the inbox reads when its row is selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadThrough {
    pub target: ReadTarget,
    pub message_id: String,
}

/// A conversation in the inbox (`inbox.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxItem {
    /// `<channel>:<group>`; the group is the channel for DMs, else the
    /// reply root or the message itself.
    pub id: String,
    pub channel_id: String,
    pub target: ReadTarget,
    /// Oldest unread message, otherwise the newest relevant one.
    pub message_id: String,
    pub latest_message_id: String,
    pub message_ids: Vec<String>,
    pub root_id: Option<String>,
    pub author_id: String,
    pub workflow_owner_id: Option<String>,
    pub preview: String,
    pub created_at: u64,
    pub mentioned: bool,
    /// Verified group members with their time and whether they mention
    /// the viewer, in `message_ids` order (Inbox archive reopening).
    pub messages: Vec<InboxMessage>,
    pub thread: bool,
    pub unread_count: u32,
    pub manual: bool,
    /// A thread prefix never acknowledges its top-level root.
    pub read_through: Vec<ReadThrough>,
}

/// One member of an inbox conversation, as the archive reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxMessage {
    pub id: String,
    pub created_at: u64,
    pub mentioned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxStatus {
    Idle,
    Loading,
    Ready,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxSnapshot {
    pub status: InboxStatus,
    pub items: Vec<InboxItem>,
    pub freshness: Freshness,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Session-driven reads
// ---------------------------------------------------------------------------

/// The startup sample (`repair`): run `filters` in order and feed each
/// result to [`Unread::repair_batch`], then call [`Unread::finish_repair`].
#[derive(Debug)]
pub struct EvidenceRepair {
    epoch: u64,
    stopped: bool,
    pub filters: Vec<Filter>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepairEnd {
    /// Roster changes asked for another repair; start one.
    pub again: bool,
    /// The sample completed; look for the homes of marked messages next.
    pub find_homes: bool,
}

/// Lookups for the channels of marked messages that are not loaded. Run
/// each filter (background); pass the events found to
/// [`Unread::homes_found`].
#[derive(Debug)]
pub struct HomesRequest {
    epoch: u64,
    ids: Vec<String>,
    pub filters: Vec<Filter>,
}

/// A parent whose conversation membership a lookup decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyTarget {
    pub parent_id: String,
    pub root_id: Option<String>,
}

/// One conversation-membership lookup (§5i.8): up to 50 parents in one
/// channel. Run `ids_filter` (structure) and the viewer's replies under
/// `reply_targets` (see [`Unread::replies_filter`], [`replies_paging`]),
/// both in the background, then [`Unread::complete_lookup`] or
/// [`Unread::fail_lookup`].
#[derive(Debug)]
pub struct LookupBatch {
    epoch: u64,
    items: Vec<(String, Queued)>,
    pub channel_id: String,
    pub ids_filter: Option<Filter>,
    pub reply_targets: Vec<ReplyTarget>,
}

/// What to do after a page of the viewer's replies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepliesPaging {
    Done,
    /// The page was full: query the first `n` targets and the rest apart.
    Split(usize),
    /// One target and a full page without a deciding reply: page back.
    Older {
        until: u64,
    },
}

/// Paging for the viewer's replies under lookup targets: a full page
/// (500 content rows) splits the targets; a single target pages back by
/// `until` (inclusive) for up to ten pages, until a reply decides it.
pub fn replies_paging(
    targets: &[ReplyTarget],
    rows: &[Verified],
    until: Option<u64>,
    pages: u32,
) -> RepliesPaging {
    let content: Vec<&Verified> = rows.iter().filter(|e| is_content(e.kind_u16())).collect();
    if content.len() < REPAIR_LIMIT {
        return RepliesPaging::Done;
    }
    if targets.len() > 1 {
        return RepliesPaging::Split(targets.len().div_ceil(2));
    }
    let oldest = content
        .iter()
        .map(|e| e.created_at_secs())
        .min()
        .unwrap_or(0);
    match targets.first() {
        Some(target)
            if !content.iter().any(|e| decides(e, target))
                && Some(oldest) != until
                && pages + 1 < LOOKUP_PAGES =>
        {
            RepliesPaging::Older { until: oldest }
        }
        _ => RepliesPaging::Done,
    }
}

/// The viewer's reply decides a lookup: it answers the parent, or is
/// anywhere in the parent's thread.
fn decides(event: &Event, target: &ReplyTarget) -> bool {
    thread_reference(event).is_some_and(|reference| {
        reference.parent_id == target.parent_id
            || reference.root_id == *target.root_id.as_ref().unwrap_or(&target.parent_id)
    })
}

/// A reading lease for one visible channel or thread surface (§5i.10).
/// Ends with [`Unread::end_reading`]; a newer local manual unread on the
/// channel, an access change or a reset invalidates it.
#[derive(Debug)]
pub struct ReadingLease {
    id: u64,
    channel_id: String,
    epoch: u64,
    manual_revision: u64,
    observed: HashSet<String>,
    dm_read: bool,
}

impl ReadingLease {
    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }
}

/// An explicit channel read with its cut captured at the click.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelReadIntent {
    pub channel_id: String,
    pub cut: u32,
    keys: Vec<String>,
    token: u64,
    has_rows: bool,
}

// ---------------------------------------------------------------------------
// The evidence owner
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Entry {
    event: Verified,
    channel_id: String,
    /// Canonical root, for replies only; `None` when the chain is broken.
    root_id: Option<String>,
    /// Direct parent; `None` for top-level messages.
    parent_id: Option<String>,
    /// The reply's marked root, which explicit follows key on.
    thread_root_id: Option<String>,
    mentioned: bool,
    broadcast: bool,
}

#[derive(Default)]
struct Index {
    entries: Vec<Entry>,
    by_channel: HashMap<String, Vec<usize>>,
    by_id: HashMap<String, usize>,
    tombstones: HashSet<String>,
    /// Retained auxiliary events by the ids they target.
    overlays: HashMap<String, Vec<String>>,
    own: HashMap<String, String>,
    joined: HashSet<String>,
    own_threads: HashSet<String>,
}

impl Index {
    fn channel(&self, channel_id: &str) -> impl Iterator<Item = &Entry> {
        self.by_channel
            .get(channel_id)
            .into_iter()
            .flatten()
            .map(|index| &self.entries[*index])
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.by_id.get(id).map(|index| &self.entries[*index])
    }
}

#[derive(Clone, Debug)]
struct Lookup {
    channel_id: String,
    done: bool,
    /// One of the viewer's messages that makes the viewer a member.
    evidence: Option<String>,
    /// The viewer has other such messages.
    more: bool,
}

#[derive(Clone, Debug)]
struct Queued {
    channel_id: String,
    parent_id: String,
    root_id: Option<String>,
    ids: Vec<String>,
    structural_only: bool,
}

struct LeaseView {
    channel_id: String,
    epoch: u64,
    manual_revision: u64,
    ids: HashSet<String>,
}

struct Cached<T> {
    stamp: u64,
    reads: u64,
    value: Arc<T>,
}

#[derive(Clone, Debug, Default)]
pub struct UnreadOptions {
    pub viewer: String,
    /// The relay's NIP-11 `self`, for trusted workflow mentions.
    pub relay_author: Option<String>,
    /// The signing authority for attributed deletions and workflow owners.
    pub authority: Option<String>,
}

/// Bounded verified evidence and its projections. No sockets, no implicit
/// reads; opening a channel or the inbox marks nothing read.
pub struct Unread<F: ThreadFollowStore = MemoryThreadFollows> {
    viewer: String,
    relay_author: Option<String>,
    /// The parsed signing authority, for attributed deletions, edits and
    /// workflow owners.
    fold: FoldOptions,
    /// `fold` with a channel's 39001 admins, for the channels the session
    /// knows them for; others fold with `fold`.
    channel_folds: HashMap<String, FoldOptions>,
    follows: F,
    choices: Lru<bool>,
    channels: Vec<ChannelInfo>,
    channel_index: HashMap<String, usize>,
    access_key: Vec<String>,
    archived_key: Vec<String>,
    channel_types: BTreeMap<String, Option<String>>,
    cached_ids: HashSet<String>,
    events: Lru<Verified>,
    bytes: usize,
    known: HashSet<String>,
    freshness: Freshness,
    error: Option<String>,
    index: Option<Index>,
    lookups: Lru<Lookup>,
    lookup_events: Lru<Verified>,
    witnesses: Lru<Verified>,
    queued: Lru<Queued>,
    retracted: Lru<()>,
    lookup_failures: u32,
    forced: HashMap<String, HashSet<String>>,
    entered: HashSet<String>,
    channel_tokens: HashMap<String, u64>,
    next_token: u64,
    epoch: u64,
    requested: bool,
    repairing: bool,
    repair_again: bool,
    asked_homes: HashSet<String>,
    leases: HashMap<u64, LeaseView>,
    next_lease: u64,
    stamp: u64,
    snapshots: HashMap<String, Cached<UnreadSnapshot>>,
    activities: HashMap<String, Cached<ThreadActivitySnapshot>>,
    inbox: Option<Cached<InboxSnapshot>>,
}

/// [`MarkCoverage`] over the current evidence, for read-state saves.
pub struct UnreadCoverage<'a, F: ThreadFollowStore> {
    unread: &'a Unread<F>,
}

impl<F: ThreadFollowStore> MarkCoverage for UnreadCoverage<'_, F> {
    /// A channel mark covers a message mark; `activity:` covers an ordinary
    /// top-level message's mark (only while the message is loaded and the
    /// channel type is known); a channel covers thread marks; a thread
    /// covers its catch-up mark. Thread marks never cover message marks:
    /// after a reload without the root a reply no longer finds its thread.
    fn covered(
        &self,
        key: &str,
        frontier: &dyn Fn(&str) -> Option<u32>,
        home: &dyn Fn(&str) -> Option<String>,
    ) -> Option<String> {
        let value = frontier(key).map_or(i64::MAX, i64::from);
        let (kind, id) = key.split_once(':')?;
        let by = |other: &str, through: i64| {
            (frontier(other).map_or(-1, i64::from) >= through).then(|| other.to_owned())
        };
        if kind == "activity" {
            return by(id, value);
        }
        if kind == "thread-activity"
            && let Some(cover) = by(&format!("thread:{id}"), value)
        {
            return Some(cover);
        }
        let index = self.unread.index.as_ref()?;
        let entry = index.entry(id);
        let channel = entry
            .map(|entry| entry.channel_id.clone())
            .or_else(|| home(id))?;
        match kind {
            "msg" => {
                let through = entry.map_or(value, |entry| entry.event.created_at_secs() as i64);
                by(&channel, through).or_else(|| {
                    entry
                        .filter(|entry| self.unread.caught_up(index, entry, frontier))
                        .map(|entry| format!("activity:{}", entry.channel_id))
                })
            }
            "thread" | "thread-activity" => by(&channel, value),
            _ => None,
        }
    }

    fn home(&self, id: &str) -> Option<String> {
        let index = self.unread.index.as_ref()?;
        index.entry(id).map(|entry| entry.channel_id.clone())
    }
}

impl Unread<MemoryThreadFollows> {
    pub fn new(options: UnreadOptions) -> Self {
        Self::with_follows(options, MemoryThreadFollows::default())
    }
}

impl<F: ThreadFollowStore> Unread<F> {
    pub fn with_follows(options: UnreadOptions, mut follows: F) -> Self {
        let mut choices = Lru::default();
        for (key, choice) in follows.read() {
            choices.insert(key, choice);
        }
        while choices.len() > THREAD_FOLLOW_LIMIT {
            choices.pop_oldest();
        }
        Self {
            viewer: options.viewer,
            relay_author: options.relay_author,
            fold: FoldOptions {
                signing_authority: options
                    .authority
                    .as_deref()
                    .and_then(|key| PublicKey::from_hex(key).ok()),
                ..Default::default()
            },
            channel_folds: HashMap::new(),
            follows,
            choices,
            channels: Vec::new(),
            channel_index: HashMap::new(),
            access_key: Vec::new(),
            archived_key: Vec::new(),
            channel_types: BTreeMap::new(),
            cached_ids: HashSet::new(),
            events: Lru::default(),
            bytes: 0,
            known: HashSet::new(),
            freshness: Freshness::Unknown,
            error: None,
            index: None,
            lookups: Lru::default(),
            lookup_events: Lru::default(),
            witnesses: Lru::default(),
            queued: Lru::default(),
            retracted: Lru::default(),
            lookup_failures: 0,
            forced: HashMap::new(),
            entered: HashSet::new(),
            channel_tokens: HashMap::new(),
            next_token: 0,
            epoch: 0,
            requested: false,
            repairing: false,
            repair_again: false,
            asked_homes: HashSet::new(),
            leases: HashMap::new(),
            next_lease: 0,
            stamp: 0,
            snapshots: HashMap::new(),
            activities: HashMap::new(),
            inbox: None,
        }
    }

    /// The access epoch: bumps on access changes, resets and staleness.
    pub fn generation(&self) -> u64 {
        self.epoch
    }

    pub fn freshness(&self) -> Freshness {
        self.freshness
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Bumps whenever any projection may have changed.
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    fn changed(&mut self) {
        self.stamp += 1;
    }

    /// The fold options for `channel`: with its admins, when known.
    fn fold_for(&self, channel: &str) -> &FoldOptions {
        self.channel_folds.get(channel).unwrap_or(&self.fold)
    }

    /// Sets a channel's admins from its relay-signed 39001, so their
    /// moderator deletes remove messages here as in the open views. An
    /// empty set keeps the reference rule (author deletes only).
    pub fn set_channel_admins(&mut self, channel: &str, admins: &HashSet<PublicKey>) {
        if self.fold_for(channel).channel_admins == *admins {
            return;
        }
        if admins.is_empty() {
            self.channel_folds.remove(channel);
        } else {
            let options = FoldOptions {
                channel_admins: admins.clone(),
                ..self.fold.clone()
            };
            self.channel_folds.insert(channel.to_owned(), options);
        }
        self.reindex();
    }

    fn channel(&self, id: &str) -> Option<&ChannelInfo> {
        self.channel_index
            .get(id)
            .map(|index| &self.channels[*index])
    }

    fn allowed(&self, id: &str) -> bool {
        self.channel(id).is_some_and(ChannelInfo::allowed)
    }

    fn is_dm(&self, id: &str) -> bool {
        self.channel(id)
            .is_some_and(|channel| channel.channel_type.as_deref() == Some("dm"))
    }

    fn structural(&self, id: &str) -> Option<&Verified> {
        self.events.get(id).or_else(|| self.lookup_events.get(id))
    }

    /// The canonical root of a content event: walk parents through retained
    /// or looked-up content in the same channel (≤32 hops).
    fn root(&self, event: &Verified) -> Option<String> {
        let channel = event.channel();
        let mut current = event;
        let mut seen = HashSet::new();
        for _ in 0..ANCESTRY_DEPTH {
            if !seen.insert(current.id) {
                return None;
            }
            let Some(reference) = thread_reference(current) else {
                return Some(current.id_hex());
            };
            let next = self.structural(&reference.root_id)?;
            if !is_content(next.kind_u16()) || next.channel() != channel {
                return None;
            }
            current = next;
        }
        None
    }

    fn workflow_owner(&self, event: &Event) -> Option<String> {
        crate::fold::workflow_owner(event, self.fold.signing_authority.as_ref())
            .map(|owner| owner.to_hex())
    }

    fn mentions_viewer(&self, event: &Event) -> bool {
        if !has_tag(event, "p", &self.viewer) {
            return false;
        }
        // The relay also p-tags a workflow's owner for attribution. Only
        // trusted kind-9 workflow output may narrow that tag.
        if event.kind.as_u16() == kinds::CHAT
            && self.relay_author.as_deref() == Some(event.pubkey.to_hex().as_str())
            && has_tag(event, "buzz:workflow", "true")
            && has_tag(event, "buzz:workflow-owner", &self.viewer)
        {
            return has_tag(event, "buzz:workflow-mention", &self.viewer);
        }
        true
    }

    fn witness(&mut self, key: &str, event: Verified) {
        let id = event.id_hex();
        let Some(lookup) = self.lookups.get_mut(key) else {
            return;
        };
        // Keep the first witness: replacing it could hide the deletion of a
        // cached parent a later lookup would reuse.
        if lookup
            .evidence
            .as_ref()
            .is_some_and(|evidence| *evidence != id)
        {
            lookup.more = true;
            return;
        }
        lookup.evidence = Some(id.clone());
        self.witnesses.insert(id, event);
        while self.witnesses.len() > WITNESSES {
            let Some((oldest, _)) = self.witnesses.pop_oldest() else {
                break;
            };
            self.lookups
                .retain(|_, lookup| lookup.evidence.as_deref() != Some(oldest.as_str()));
        }
    }

    fn ensure_index(&mut self) {
        if self.index.is_some() {
            return;
        }
        let mut index = Index::default();
        for (aux_id, event) in self.events.iter() {
            if !is_auxiliary(event.kind_u16()) {
                continue;
            }
            for id in e_targets(event) {
                index
                    .overlays
                    .entry(id.to_owned())
                    .or_default()
                    .push(aux_id.clone());
            }
            if !matches!(event.kind_u16(), kinds::DELETION | kinds::MODERATOR_DELETE) {
                continue;
            }
            for id in e_targets(event) {
                if let Some(target) = self.events.get(id)
                    && let channel = target.channel().unwrap_or_default()
                    && crate::fold::deletion_applies(event, target, channel, self.fold_for(channel))
                {
                    index.tombstones.insert(target.id_hex());
                }
            }
        }
        let mut witnessed = Vec::new();
        for (id, event) in self.events.iter() {
            if !is_content(event.kind_u16()) || index.tombstones.contains(id) {
                continue;
            }
            let Some(channel) = event.channel() else {
                continue;
            };
            let root = self.root(event);
            let reference = thread_reference(event);
            let parent_id = reference.as_ref().map(|r| r.parent_id.clone());
            if event.pubkey.to_hex() == self.viewer {
                index.own.insert(id.clone(), channel.to_owned());
                let thread = reference
                    .as_ref()
                    .map_or(id.as_str(), |r| r.root_id.as_str());
                index.own_threads.insert(conversation_key(channel, thread));
                if let Some(parent) = &parent_id {
                    let key = conversation_key(channel, parent);
                    index.joined.insert(key.clone());
                    // A later reply of the viewer outlives the window.
                    if self.lookups.get(&key).is_some_and(|lookup| lookup.done) {
                        witnessed.push((key, event.clone()));
                    }
                }
            }
            let position = index.entries.len();
            index.entries.push(Entry {
                event: event.clone(),
                channel_id: channel.to_owned(),
                root_id: parent_id.as_ref().and(root),
                thread_root_id: reference.map(|r| r.root_id),
                parent_id,
                mentioned: self.mentions_viewer(event),
                broadcast: is_broadcast(event),
            });
            index
                .by_channel
                .entry(channel.to_owned())
                .or_default()
                .push(position);
            index.by_id.insert(id.clone(), position);
        }
        for (key, event) in witnessed {
            self.witness(&key, event);
        }
        for (_, lookup) in self.lookups.iter() {
            if let Some(event) = lookup
                .evidence
                .as_ref()
                .and_then(|evidence| self.witnesses.get(evidence))
            {
                let thread = thread_reference(event).map_or_else(|| event.id_hex(), |r| r.root_id);
                index
                    .own_threads
                    .insert(conversation_key(&lookup.channel_id, &thread));
            }
        }
        self.index = Some(index);
    }

    fn reindex(&mut self) {
        self.index = None;
        self.changed();
    }

    // -- predicates ---------------------------------------------------------

    fn chosen(&self, entry: &Entry) -> Option<bool> {
        let root = entry.thread_root_id.as_ref()?;
        self.choices
            .get(&conversation_key(&entry.channel_id, root))
            .copied()
    }

    /// The reply is in a conversation the viewer is part of: an explicit
    /// follow choice, else the viewer wrote or replied anywhere in its
    /// thread, wrote its parent, or replied to the same parent.
    fn conversation(&self, index: &Index, entry: &Entry) -> bool {
        let Some(parent) = &entry.parent_id else {
            return false;
        };
        if let Some(explicit) = self.chosen(entry) {
            return explicit;
        }
        entry.thread_root_id.as_ref().is_some_and(|root| {
            index
                .own_threads
                .contains(&conversation_key(&entry.channel_id, root))
        }) || index.own.get(parent) == Some(&entry.channel_id)
            || index
                .joined
                .contains(&conversation_key(&entry.channel_id, parent))
    }

    /// Top-level posts always count; a reply only in a DM, when it mentions
    /// the viewer, is a broadcast, or is in the viewer's conversation.
    fn relevant(&self, index: &Index, entry: &Entry, dm: bool) -> bool {
        entry.parent_id.is_none()
            || dm
            || entry.mentioned
            || entry.broadcast
            || self.conversation(index, entry)
    }

    /// Retained evidence cannot decide this reply's conversation yet.
    fn undecided(&self, index: &Index, entry: &Entry, dm: bool) -> bool {
        let Some(parent) = &entry.parent_id else {
            return false;
        };
        !self.relevant(index, entry, dm)
            && self.chosen(entry).is_none()
            && !self
                .lookups
                .get(&conversation_key(&entry.channel_id, parent))
                .is_some_and(|lookup| lookup.done)
    }

    fn category(&self, index: &Index, entry: &Entry, dm: bool) -> Option<Category> {
        // DMs p-tag the recipient, so direct wins inside DM channels.
        if dm {
            Some(Category::Direct)
        } else if entry.mentioned {
            Some(Category::Mention)
        } else if self.conversation(index, entry) {
            Some(Category::Thread)
        } else {
            None
        }
    }

    fn priority(&self, index: &Index, entry: &Entry, dm: bool) -> bool {
        self.category(index, entry, dm).is_some() || entry.broadcast
    }

    /// An ordinary top-level message, which channel catch-up reads.
    fn timeline(&self, index: &Index, entry: &Entry, dm: bool) -> bool {
        thread_reference(&entry.event).is_none() && !self.priority(index, entry, dm)
    }

    /// The channel's `activity:` mark reads this message. Needs the
    /// channel's known type: catch-up never reads DM messages.
    fn caught_up(
        &self,
        index: &Index,
        entry: &Entry,
        frontier: &dyn Fn(&str) -> Option<u32>,
    ) -> bool {
        let Some(channel_type) = self
            .channel(&entry.channel_id)
            .and_then(|channel| channel.channel_type.as_deref())
        else {
            return false;
        };
        self.timeline(index, entry, channel_type == "dm")
            && frontier(&format!("activity:{}", entry.channel_id)).map_or(-1, i64::from)
                >= entry.event.created_at_secs() as i64
    }

    /// The read-state half of unread (`afterFrontier`). Same-second is
    /// read: unread means strictly after.
    fn after_frontier(&self, index: &Index, entry: &Entry, state: &ReadState, dm: bool) -> bool {
        let id = entry.event.id_hex();
        let message = format!("msg:{id}");
        let channel = entry.channel_id.as_str();
        let root = entry.root_id.as_deref();
        let frontier = effective_frontier(state, &message, Some(channel), root);
        let forced = override_active(state.overrides.get(&message), frontier)
            || override_active(
                state.overrides.get(channel),
                effective_frontier(state, channel, None, None),
            )
            || root.is_some_and(|root| {
                let thread = format!("thread:{root}");
                override_active(
                    state.overrides.get(&thread),
                    effective_frontier(state, &thread, Some(channel), None),
                )
            });
        // Channel catch-up only reads ordinary top-level messages.
        let ordinary = if self.timeline(index, entry, dm) {
            state.frontier(&format!("activity:{channel}"))
        } else {
            None
        };
        let thread = root.and_then(|root| state.frontier(&format!("thread-activity:{root}")));
        let caught_up = [frontier, ordinary, thread]
            .into_iter()
            .map(|value| value.map_or(-1, i64::from))
            .max()
            .unwrap_or(-1);
        entry.event.created_at_secs() as i64 > caught_up || forced
    }

    fn is_unread(&self, index: &Index, entry: &Entry, reads: &dyn ReadLookup, dm: bool) -> bool {
        if entry.event.pubkey.to_hex() == self.viewer {
            return false;
        }
        let id = entry.event.id_hex();
        if self
            .forced
            .get(&entry.channel_id)
            .is_some_and(|forced| forced.contains(&id))
            || reads.local_unread(&format!("msg:{id}")).is_some()
        {
            return true;
        }
        self.relevant(index, entry, dm) && self.after_frontier(index, entry, reads.state(), dm)
    }

    /// The lookup a selector would queue for this reply (`want`).
    fn want(
        &self,
        index: &Index,
        entry: &Entry,
        dm: bool,
        state: &ReadState,
    ) -> Option<(String, Queued)> {
        let parent = entry.parent_id.as_ref()?;
        if entry.event.pubkey.to_hex() == self.viewer {
            return None;
        }
        let channel = &entry.channel_id;
        let structural_root = (self.chosen(entry) == Some(true) && entry.root_id.is_none())
            .then(|| entry.thread_root_id.clone())
            .flatten();
        let key = match &structural_root {
            Some(root) => format!("{}:structure", conversation_key(channel, root)),
            None => conversation_key(channel, parent),
        };
        if self.lookups.contains(&key)
            || (structural_root.is_none()
                && (!self.undecided(index, entry, dm)
                    || !self.after_frontier(index, entry, state, dm)))
        {
            return None;
        }
        let root_id = entry.thread_root_id.clone().filter(|root| root != parent);
        let mut ids = vec![structural_root.clone().unwrap_or_else(|| parent.clone())];
        if let Some(root) = &root_id
            && self.structural(root).is_none()
            && !ids.contains(root)
        {
            ids.push(root.clone());
        }
        Some((
            key,
            Queued {
                channel_id: channel.clone(),
                parent_id: structural_root.clone().unwrap_or_else(|| parent.clone()),
                root_id,
                ids,
                structural_only: structural_root.is_some(),
            },
        ))
    }

    fn queue(&mut self, wants: Vec<(String, Queued)>) {
        for (key, item) in wants {
            if self.lookups.contains(&key) {
                continue;
            }
            self.lookups.insert(
                key.clone(),
                Lookup {
                    channel_id: item.channel_id.clone(),
                    done: false,
                    evidence: None,
                    more: false,
                },
            );
            self.queued.insert(key, item);
        }
    }

    // -- projections --------------------------------------------------------

    fn compute(
        &self,
        target: &ReadTarget,
        reads: &dyn ReadLookup,
    ) -> (UnreadSnapshot, Vec<(String, Queued)>) {
        let index = self.index.as_ref().expect("indexed");
        let channel_id = target.channel_id();
        let Ok(key) = target.key() else {
            return (UnreadSnapshot::inaccessible(target.clone()), Vec::new());
        };
        // Thread and message targets need their root or message retained
        // (or looked up) in the same channel.
        let same_channel =
            |event: &Verified| event.channel() == Some(channel_id) && is_content(event.kind_u16());
        let accessible = self.allowed(channel_id)
            && match target {
                ReadTarget::Channel { .. } => true,
                ReadTarget::Thread { root_id, .. } => {
                    self.structural(root_id).is_some_and(same_channel)
                }
                ReadTarget::Message { message_id, .. } => {
                    self.events.get(message_id).is_some_and(same_channel)
                }
            };
        if !accessible {
            return (UnreadSnapshot::inaccessible(target.clone()), Vec::new());
        }
        let evidence = self.known.contains(channel_id);
        let state = reads.state();
        let dm = self.is_dm(channel_id);
        let (mut count, mut attention, mut mentions) = (0, 0, 0);
        let mut latest: Option<&Verified> = None;
        let mut shown: Option<&Verified> = None;
        let mut wants = Vec::new();
        let newer = |event: &Verified, than: Option<&Verified>| {
            than.is_none_or(|than| {
                event.created_at > than.created_at
                    || (event.created_at == than.created_at && event.id < than.id)
            })
        };
        for entry in index.channel(channel_id) {
            let event = &entry.event;
            if matches!(target, ReadTarget::Channel { .. }) {
                if newer(event, latest) {
                    latest = Some(event);
                }
                if (entry.parent_id.is_none() || entry.broadcast) && newer(event, shown) {
                    shown = Some(event);
                }
            }
            let in_target = match target {
                ReadTarget::Channel { .. } => true,
                ReadTarget::Message { message_id, .. } => event.id_hex() == *message_id,
                ReadTarget::Thread { root_id, .. } => entry.root_id.as_ref() == Some(root_id),
            };
            if !in_target {
                continue;
            }
            wants.extend(self.want(index, entry, dm, state));
            if !self.is_unread(index, entry, reads, dm) {
                continue;
            }
            count += 1;
            if self.priority(index, entry, dm) {
                attention += 1;
            }
            // DM events p-tag the recipient: direct wins inside DMs, so a
            // direct message is never also a mention.
            if entry.mentioned && !dm {
                mentions += 1;
            }
        }
        let manual = if reads.local_unread(&key).is_some()
            || matches!(target, ReadTarget::Message { message_id, .. }
                if self.forced.get(channel_id).is_some_and(|forced| forced.contains(message_id)))
            || (matches!(target, ReadTarget::Channel { .. })
                && reads.local_unread(&message_force_key(channel_id)).is_some())
        {
            Manual::LocalOnly
        } else if override_active(
            state.overrides.get(&key),
            effective_frontier(state, &key, Some(channel_id), None),
        ) {
            Manual::Remote
        } else {
            Manual::None
        };
        let snapshot = UnreadSnapshot {
            target: target.clone(),
            latest_message: latest.map(|event| LatestMessage {
                id: event.id_hex(),
                created_at: event.created_at_secs(),
                preview: shown.map_or_else(String::new, |shown| self.shown_preview(shown)),
            }),
            observed_count: evidence.then_some(count),
            attention_count: evidence.then_some(attention),
            mention_count: evidence.then_some(mentions),
            coverage: if evidence {
                Coverage::Observed
            } else {
                Coverage::Unknown
            },
            freshness: self.freshness,
            manual,
            error: self.error.clone(),
        };
        (snapshot, wants)
    }

    /// A timeline message's preview, edits applied.
    fn shown_preview(&self, event: &Verified) -> String {
        if event.kind_u16() == kinds::DIFF {
            return latest_preview(event);
        }
        let channel = event.channel().unwrap_or_default();
        self.folded_previews(channel, std::iter::once(event))
            .remove(&event.id)
            .unwrap_or_else(|| latest_preview(event))
    }

    /// The unread state of a target. Unchanged values keep their `Arc`.
    pub fn snapshot(&mut self, target: &ReadTarget, reads: &dyn ReadLookup) -> Arc<UnreadSnapshot> {
        self.ensure_index();
        let key = format!(
            "{}:{}",
            target.channel_id(),
            target.key().unwrap_or_default()
        );
        let (stamp, changes) = (self.stamp, reads.changes());
        if let Some(cached) = self.snapshots.get(&key)
            && cached.stamp == stamp
            && cached.reads == changes
            && cached.value.target == *target
        {
            return cached.value.clone();
        }
        let (value, wants) = self.compute(target, reads);
        self.queue(wants);
        let value = match self.snapshots.get(&key) {
            Some(cached) if *cached.value == value => cached.value.clone(),
            _ => Arc::new(value),
        };
        if !self.snapshots.contains_key(&key) && self.snapshots.len() >= SNAPSHOT_CACHE {
            self.snapshots.clear();
        }
        self.snapshots.insert(
            key,
            Cached {
                stamp,
                reads: changes,
                value: value.clone(),
            },
        );
        value
    }

    /// Whether this event is retained evidence already.
    pub fn retains(&self, id: &str) -> bool {
        self.events.contains(id)
    }

    /// Retained edits of `ids` by those messages' authors
    /// (`retainedEditIds`), so the inbox history also checks their
    /// deletions.
    pub fn retained_edit_ids(&mut self, ids: &[String]) -> Vec<String> {
        let targets: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut memo = HashMap::new();
        let mut candidates = Vec::new();
        for (_, event) in self.events.iter() {
            if event.kind_u16() != kinds::EDIT {
                continue;
            }
            let find = |id: &str| self.events.get(id).cloned();
            let Some(channels) = owners(event, &find, &mut memo, &mut HashSet::new()) else {
                continue;
            };
            if channels.iter().any(|channel| !self.allowed(channel)) {
                continue;
            }
            let target = e_targets(event).into_iter().find_map(|id| {
                targets
                    .contains(id)
                    .then(|| self.events.get(id))
                    .flatten()
                    .filter(|target| is_content(target.kind_u16()) && target.pubkey == event.pubkey)
                    .cloned()
            });
            if let Some(target) = target {
                candidates.push((event.clone(), target));
            }
        }
        candidates
            .into_iter()
            .filter(|(edit, target)| !self.deleted(edit) && !self.deleted(target))
            .map(|(edit, _)| edit.id_hex())
            .collect()
    }

    /// A retained evidence event (`unread.event`), for visibility checks.
    pub fn event(&self, id: &str) -> Option<&Verified> {
        self.events.get(id).or_else(|| self.lookup_events.get(id))
    }

    /// One message's attention (badges' policy; the notification input).
    pub fn attention(
        &mut self,
        channel_id: &str,
        message_id: &str,
        reads: &dyn ReadLookup,
    ) -> MessageAttention {
        if !self.allowed(channel_id) {
            return MessageAttention::unknown();
        }
        self.ensure_index();
        let Some(event) = self.events.get(message_id) else {
            return MessageAttention::unknown();
        };
        if event.channel() != Some(channel_id) || !is_content(event.kind_u16()) {
            return MessageAttention::unknown();
        }
        let index = self.index.as_ref().expect("indexed");
        let forced = self
            .forced
            .get(channel_id)
            .is_some_and(|forced| forced.contains(message_id));
        let entry = index
            .entry(message_id)
            .filter(|entry| entry.channel_id == channel_id);
        let Some(entry) = entry.filter(|_| event.pubkey.to_hex() != self.viewer) else {
            return MessageAttention {
                status: AttentionStatus::Ineligible,
                forced,
                ..MessageAttention::unknown()
            };
        };
        let dm = self.is_dm(channel_id);
        let category = self.category(index, entry, dm);
        let open = self.undecided(index, entry, dm);
        let want = if open {
            self.want(index, entry, dm, reads.state())
        } else {
            None
        };
        let viewing = self
            .leases
            .values()
            .any(|view| view.ids.contains(message_id) && self.view_valid(view, reads));
        let unknown_root = thread_reference(event).is_some() && entry.root_id.is_none();
        let mut attention = MessageAttention {
            status: if category.is_some() {
                AttentionStatus::Eligible
            } else if open || unknown_root {
                AttentionStatus::Unknown
            } else {
                AttentionStatus::Ineligible
            },
            category,
            mentioned: entry.mentioned,
            root_id: entry.root_id.clone(),
            pending: false,
            unread: self.is_unread(index, entry, reads, dm),
            forced,
            viewing,
        };
        let parent_key = conversation_key(channel_id, entry.parent_id.as_deref().unwrap_or(""));
        self.queue(want.into_iter().collect());
        attention.pending = open
            && self
                .lookups
                .get(&parent_key)
                .is_some_and(|lookup| !lookup.done);
        attention
    }

    /// The folded text of `messages` in `channel_id`, replies included.
    fn folded_previews<'a>(
        &self,
        channel_id: &str,
        messages: impl Iterator<Item = &'a Verified>,
    ) -> HashMap<nostr::prelude::EventId, String> {
        // Evidence holds no relay-signed summaries or system rows, the only
        // events the relay key decides; without any key, previews stay raw.
        let Some(relay) = self
            .relay_author
            .as_deref()
            .and_then(|key| PublicKey::from_hex(key).ok())
            .or(self.fold.signing_authority)
        else {
            return HashMap::new();
        };
        // The messages and their overlays, including overlays of overlays
        // (a deletion of an edit).
        let index = self.index.as_ref().expect("indexed");
        let mut events: Vec<Verified> = messages.cloned().collect();
        let mut seen = HashSet::new();
        let mut next = 0;
        while next < events.len() {
            let id = events[next].id_hex();
            next += 1;
            for aux in index.overlays.get(&id).into_iter().flatten() {
                if seen.insert(aux.as_str())
                    && let Some(event) = self.events.get(aux)
                {
                    events.push(event.clone());
                }
            }
        }
        let options = FoldOptions {
            include_replies: true,
            ..self.fold_for(channel_id).clone()
        };
        crate::fold::fold_messages(channel_id, &relay, &events, &options)
            .into_iter()
            .map(|row| (row.id, row.content.clone()))
            .collect()
    }

    fn compute_activity(
        &self,
        channel_id: &str,
        reads: &dyn ReadLookup,
    ) -> (ThreadActivitySnapshot, Vec<(String, Queued)>) {
        if !self.allowed(channel_id) || !self.known.contains(channel_id) {
            return (
                ThreadActivitySnapshot {
                    channel_id: channel_id.to_owned(),
                    items: None,
                    coverage: Coverage::Unknown,
                    freshness: Freshness::Unknown,
                    error: None,
                },
                Vec::new(),
            );
        }
        let index = self.index.as_ref().expect("indexed");
        let dm = self.is_dm(channel_id);
        let state = reads.state();
        let mut grouped: BTreeMap<String, ThreadActivityItem> = BTreeMap::new();
        let mut wants = Vec::new();
        // Previews show the folded text, edits applied; folded only when
        // something unread is presented, which most publishes lack.
        let mut presented: Option<HashMap<nostr::prelude::EventId, String>> = None;
        for entry in index.channel(channel_id) {
            wants.extend(self.want(index, entry, dm, state));
            let Some(root) = &entry.root_id else { continue };
            if (!entry.mentioned && !entry.broadcast && !self.conversation(index, entry))
                || !self.is_unread(index, entry, reads, dm)
            {
                continue;
            }
            let event = &entry.event;
            let presented = presented.get_or_insert_with(|| {
                let messages = index.channel(channel_id).map(|entry| &entry.event);
                self.folded_previews(channel_id, messages)
            });
            let item = ThreadActivityItem {
                channel_id: channel_id.to_owned(),
                root_id: root.clone(),
                latest_message_id: event.id_hex(),
                author_id: event.pubkey_hex(),
                workflow_owner_id: self.workflow_owner(event),
                created_at: event.created_at_secs(),
                preview: presented
                    .get(&event.id)
                    .cloned()
                    .unwrap_or_else(|| preview_text(event)),
                unread_count: 1,
            };
            match grouped.get_mut(root) {
                None => {
                    grouped.insert(root.clone(), item);
                }
                Some(current) => {
                    let count = current.unread_count + 1;
                    if item.created_at > current.created_at
                        || (item.created_at == current.created_at
                            && item.latest_message_id < current.latest_message_id)
                    {
                        *current = item;
                    }
                    current.unread_count = count;
                }
            }
        }
        let mut items: Vec<ThreadActivityItem> = grouped.into_values().collect();
        items.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.latest_message_id.cmp(&b.latest_message_id))
        });
        (
            ThreadActivitySnapshot {
                channel_id: channel_id.to_owned(),
                items: Some(items),
                coverage: Coverage::Observed,
                freshness: self.freshness,
                error: self.error.clone(),
            },
            wants,
        )
    }

    /// Unread replies in followed threads, mentions and broadcasts, grouped
    /// by root (`computeActivity`), newest first.
    pub fn activity(
        &mut self,
        channel_id: &str,
        reads: &dyn ReadLookup,
    ) -> Arc<ThreadActivitySnapshot> {
        self.ensure_index();
        let (stamp, changes) = (self.stamp, reads.changes());
        if let Some(cached) = self.activities.get(channel_id)
            && cached.stamp == stamp
            && cached.reads == changes
        {
            return cached.value.clone();
        }
        let (value, wants) = self.compute_activity(channel_id, reads);
        self.queue(wants);
        let value = match self.activities.get(channel_id) {
            Some(cached) if *cached.value == value => cached.value.clone(),
            _ => Arc::new(value),
        };
        if !self.activities.contains_key(channel_id) && self.activities.len() >= SNAPSHOT_CACHE {
            self.activities.clear();
        }
        self.activities.insert(
            channel_id.to_owned(),
            Cached {
                stamp,
                reads: changes,
                value: value.clone(),
            },
        );
        value
    }

    fn compute_inbox(&self, reads: &dyn ReadLookup) -> (InboxSnapshot, Vec<(String, Queued)>) {
        let index = self.index.as_ref().expect("indexed");
        let state = reads.state();
        let mut items = Vec::new();
        let mut wants = Vec::new();
        for channel in &self.channels {
            if channel.cached || channel.archived || !channel.member {
                continue;
            }
            let dm = channel.channel_type.as_deref() == Some("dm");
            let mut groups: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
            for entry in index.channel(&channel.id) {
                wants.extend(self.want(index, entry, dm, state));
                if entry.event.pubkey.to_hex() == self.viewer
                    || self.category(index, entry, dm).is_none()
                {
                    continue;
                }
                let group = if dm {
                    channel.id.clone()
                } else {
                    entry
                        .root_id
                        .clone()
                        .unwrap_or_else(|| entry.event.id_hex())
                };
                groups.entry(group).or_default().push(entry);
            }
            for (group, mut entries) in groups {
                entries.sort_by(|a, b| {
                    a.event
                        .created_at
                        .cmp(&b.event.created_at)
                        .then_with(|| a.event.id.cmp(&b.event.id))
                });
                let Some(latest) = entries.last().copied() else {
                    continue;
                };
                let unread: Vec<&Entry> = entries
                    .iter()
                    .copied()
                    .filter(|entry| self.is_unread(index, entry, reads, dm))
                    .collect();
                let representative = unread.first().copied().unwrap_or(latest);
                let last_reply = entries.iter().rev().find(|entry| entry.root_id.is_some());
                let reply_root = last_reply.and_then(|reply| reply.root_id.clone());
                let target = if dm {
                    ReadTarget::channel(&channel.id)
                } else {
                    match &reply_root {
                        Some(root)
                            if self.events.contains(root) && !index.tombstones.contains(root) =>
                        {
                            ReadTarget::thread(&channel.id, root)
                        }
                        _ => ReadTarget::message(&channel.id, latest.event.id_hex()),
                    }
                };
                let mut read_through: Vec<ReadThrough> = if dm {
                    Vec::new()
                } else {
                    entries
                        .iter()
                        .filter(|entry| {
                            entry.root_id.is_none()
                                || reads
                                    .local_unread(&format!("msg:{}", entry.event.id_hex()))
                                    .is_some()
                        })
                        .map(|entry| ReadThrough {
                            target: ReadTarget::message(&channel.id, entry.event.id_hex()),
                            message_id: entry.event.id_hex(),
                        })
                        .collect()
                };
                if !dm && let (Some(reply), Some(root)) = (last_reply, &reply_root) {
                    read_through.push(ReadThrough {
                        target: ReadTarget::thread(&channel.id, root),
                        message_id: reply.event.id_hex(),
                    });
                }
                let marked = |target: &ReadTarget| {
                    target
                        .key()
                        .is_ok_and(|key| reads.local_unread(&key).is_some())
                };
                let manual =
                    marked(&target) || read_through.iter().any(|step| marked(&step.target));
                let event = &representative.event;
                items.push(InboxItem {
                    id: format!("{}:{group}", channel.id),
                    channel_id: channel.id.clone(),
                    target,
                    message_id: event.id_hex(),
                    latest_message_id: latest.event.id_hex(),
                    message_ids: entries.iter().map(|entry| entry.event.id_hex()).collect(),
                    root_id: representative.root_id.clone(),
                    author_id: event.pubkey_hex(),
                    workflow_owner_id: self.workflow_owner(event),
                    preview: preview_text(event),
                    created_at: latest.event.created_at_secs(),
                    mentioned: entries.iter().any(|entry| entry.mentioned),
                    messages: entries
                        .iter()
                        .map(|entry| InboxMessage {
                            id: entry.event.id_hex(),
                            created_at: entry.event.created_at_secs(),
                            mentioned: entry.mentioned,
                        })
                        .collect(),
                    thread: entries.iter().any(|entry| entry.parent_id.is_some()),
                    unread_count: unread.len() as u32,
                    manual,
                    read_through,
                });
            }
        }
        items.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let status = if self.error.is_some() {
            InboxStatus::Error
        } else if self.repairing {
            InboxStatus::Loading
        } else if self.freshness == Freshness::Unknown {
            InboxStatus::Idle
        } else {
            InboxStatus::Ready
        };
        (
            InboxSnapshot {
                status,
                items,
                freshness: self.freshness,
                error: self.error.clone(),
            },
            wants,
        )
    }

    /// The inbox (§5l.5): mention, direct and followed-thread
    /// conversations from retained evidence, newest first. Archived
    /// channels are hidden but keep their evidence and read state.
    pub fn inbox(&mut self, reads: &dyn ReadLookup) -> Arc<InboxSnapshot> {
        self.ensure_index();
        let (stamp, changes) = (self.stamp, reads.changes());
        if let Some(cached) = &self.inbox
            && cached.stamp == stamp
            && cached.reads == changes
        {
            return cached.value.clone();
        }
        let (value, wants) = self.compute_inbox(reads);
        self.queue(wants);
        let value = match &self.inbox {
            Some(cached) if *cached.value == value => cached.value.clone(),
            _ => Arc::new(value),
        };
        self.inbox = Some(Cached {
            stamp,
            reads: changes,
            value: value.clone(),
        });
        value
    }

    /// Evidence coverage for read-state saves and retention.
    pub fn coverage(&mut self) -> UnreadCoverage<'_, F> {
        self.ensure_index();
        UnreadCoverage { unread: self }
    }

    // -- evidence -----------------------------------------------------------

    /// The viewer's deletions end lookup memberships they were evidence
    /// for, even when the deletion itself is not retained.
    fn retract(&mut self, batch: &[Verified]) {
        let ids: Vec<String> = batch
            .iter()
            .filter(|event| {
                matches!(event.kind_u16(), kinds::DELETION | kinds::MODERATOR_DELETE)
                    && event.pubkey_hex() == self.viewer
            })
            .flat_map(|event| {
                e_targets(event)
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        if ids.is_empty() {
            return;
        }
        let mut changed = false;
        for id in ids {
            self.retracted.insert(id.clone(), ());
            self.witnesses.remove(&id);
            // A deleted message must be fetched again, not reused. Roots
            // may have walked through it.
            changed |= self.lookup_events.remove(&id).is_some();
            let affected: Vec<(String, bool)> = self
                .lookups
                .iter()
                .filter(|(_, lookup)| lookup.evidence.as_deref() == Some(id.as_str()))
                .map(|(key, lookup)| (key.clone(), lookup.more))
                .collect();
            for (key, more) in affected {
                changed = true;
                // Other messages may still hold the membership: ask again.
                if more {
                    self.lookups.remove(&key);
                } else if let Some(lookup) = self.lookups.get_mut(&key) {
                    lookup.evidence = None;
                }
            }
        }
        while self.retracted.len() > RETRACTED {
            self.retracted.pop_oldest();
        }
        if changed {
            self.reindex();
        }
    }

    /// Feeds a verified batch from any read, live route or restored cache.
    /// Content (9, 40002, 40008) and auxiliary (40003, 5, 9005) events are
    /// retained when every channel owning them is accessible. Returns false
    /// when the bound overflowed, which clears all evidence.
    pub fn accept(&mut self, batch: &[Verified]) -> bool {
        self.retract(batch);
        let incoming: HashMap<String, Verified> = batch
            .iter()
            .map(|event| (event.id_hex(), event.clone()))
            .collect();
        let mut memo = HashMap::new();
        let mut changed = false;
        for event in batch {
            let id = event.id_hex();
            if !(is_content(event.kind_u16()) || is_auxiliary(event.kind_u16()))
                || self.events.contains(&id)
            {
                continue;
            }
            let find = |id: &str| incoming.get(id).or_else(|| self.events.get(id)).cloned();
            let Some(channels) = owners(event, &find, &mut memo, &mut HashSet::new()) else {
                continue;
            };
            if channels.iter().any(|channel| !self.allowed(channel)) {
                continue;
            }
            let size = event.byte_size();
            if self.events.len() >= EVIDENCE_EVENTS || self.bytes + size > EVIDENCE_BYTES {
                self.events.clear();
                self.known.clear();
                self.bytes = 0;
                self.error = Some("Unread observation capacity reached; refresh available".into());
                self.freshness = Freshness::Stale;
                self.reindex();
                return false;
            }
            self.events.insert(id, event.clone());
            self.bytes += size;
            self.known.extend(channels);
            changed = true;
        }
        if changed {
            self.freshness = Freshness::Observed;
            self.reindex();
        }
        true
    }

    /// Replaces the channel list. Returns true when the session should
    /// start an evidence repair: channels confirmed from a cached roster
    /// still owe evidence (§5i.7).
    pub fn set_channels(&mut self, channels: Vec<ChannelInfo>) -> bool {
        let types: BTreeMap<String, Option<String>> = channels
            .iter()
            .filter(|channel| channel.allowed())
            .map(|channel| (channel.id.clone(), channel.channel_type.clone()))
            .collect();
        let access: Vec<String> = types.keys().cloned().collect();
        let types_changed = types
            .iter()
            .any(|(id, kind)| self.channel_types.get(id).is_some_and(|old| old != kind));
        let confirmed = access.iter().any(|id| self.cached_ids.contains(id));
        let mut archived: Vec<String> = channels
            .iter()
            .filter(|channel| channel.archived && channel.allowed())
            .map(|channel| channel.id.clone())
            .collect();
        archived.sort();
        let archive_changed = archived != self.archived_key;
        self.cached_ids = channels
            .iter()
            .filter(|channel| channel.cached)
            .map(|channel| channel.id.clone())
            .collect();
        self.channel_index = channels
            .iter()
            .enumerate()
            .map(|(index, channel)| (channel.id.clone(), index))
            .collect();
        self.channels = channels;
        self.channel_types = types;
        self.archived_key = archived;
        if access == self.access_key {
            if types_changed || archive_changed {
                self.reindex();
            }
            return false;
        }
        self.access_key = access;
        self.purge();
        if self.requested && confirmed {
            self.repair_again = true;
            return !self.repairing;
        }
        false
    }

    /// Drops everything about channels that are no longer accessible.
    fn purge(&mut self) {
        // A revoke/regrant must not revive intent from the old epoch.
        self.epoch += 1;
        let channels: HashSet<String> = self
            .channels
            .iter()
            .filter(|channel| channel.allowed())
            .map(|channel| channel.id.clone())
            .collect();
        let allowed = |id: &str| channels.contains(id);
        self.channel_tokens.retain(|id, _| allowed(id));
        self.channel_folds.retain(|id, _| allowed(id));
        self.lookups.retain(|_, lookup| allowed(&lookup.channel_id));
        self.lookup_events
            .retain(|_, event| event.channel().is_some_and(allowed));
        self.witnesses
            .retain(|_, event| event.channel().is_some_and(allowed));
        self.queued.retain(|_, item| allowed(&item.channel_id));
        self.known.retain(|id| allowed(id));
        self.forced.retain(|id, _| allowed(id));
        self.entered.retain(|id| allowed(id));
        let retained = self.events.clone();
        let find = |id: &str| retained.get(id).cloned();
        let mut memo = HashMap::new();
        self.events.retain(|_, event| {
            owners(event, &find, &mut memo, &mut HashSet::new())
                .is_some_and(|owners| owners.iter().all(|channel| allowed(channel)))
        });
        self.bytes = self.events.iter().map(|(_, event)| event.byte_size()).sum();
        self.reindex();
    }

    /// The connection went stale: fence queued explicit intent. The session
    /// also calls [`ReadStateSync::stale`].
    pub fn stale(&mut self) {
        self.epoch += 1;
        self.channel_tokens.clear();
        self.freshness = Freshness::Stale;
        self.changed();
    }

    /// Clears all evidence (cache clear or account change).
    pub fn clear(&mut self) {
        self.epoch += 1;
        self.channel_tokens.clear();
        self.lookups.clear();
        self.lookup_events.clear();
        self.queued.clear();
        self.retracted.clear();
        self.witnesses.clear();
        self.lookup_failures = 0;
        self.repair_again = false;
        self.events.clear();
        self.known.clear();
        self.forced.clear();
        self.entered.clear();
        self.bytes = 0;
        self.freshness = Freshness::Unknown;
        self.error = None;
        self.reindex();
    }

    /// Whether a reconnect should refresh read state and repair evidence
    /// (in the background): only once evidence was requested.
    pub fn reconnect(&self) -> bool {
        self.requested
    }

    /// `ensure()`: true until the first repair was requested.
    pub fn needs_repair(&self) -> bool {
        !self.requested
    }

    // -- repair -------------------------------------------------------------

    /// Starts the sampled evidence read over every accessible channel. Call
    /// after read state has loaded (`reads.ensure()`). `None` while one runs.
    pub fn begin_repair(&mut self) -> Option<EvidenceRepair> {
        self.requested = true;
        if self.repairing {
            return None;
        }
        self.repair_again = false;
        self.repairing = true;
        self.changed();
        let ids: Vec<String> = self
            .channels
            .iter()
            .filter(|channel| channel.allowed())
            .map(|channel| channel.id.clone())
            .collect();
        Some(EvidenceRepair {
            epoch: self.epoch,
            stopped: false,
            filters: evidence_filters(&ids),
        })
    }

    /// Feeds one batch's events (publishing progressively). Returns false
    /// when the repair must stop (capacity or a reset).
    pub fn repair_batch(&mut self, repair: &mut EvidenceRepair, events: &[Verified]) -> bool {
        if repair.stopped || repair.epoch != self.epoch || !self.accept(events) {
            repair.stopped = true;
            return false;
        }
        true
    }

    /// Ends the repair. A failed read leaves evidence stale with its error
    /// until an explicit refresh or a reconnect.
    pub fn finish_repair(
        &mut self,
        repair: EvidenceRepair,
        result: std::result::Result<(), String>,
    ) -> RepairEnd {
        let current = repair.epoch == self.epoch && !repair.stopped;
        let ok = current && result.is_ok();
        if current {
            match result {
                Ok(()) => {
                    self.freshness = Freshness::Observed;
                    self.error = None;
                }
                Err(error) => {
                    self.freshness = Freshness::Stale;
                    self.error = Some(error);
                }
            }
        }
        self.repairing = false;
        self.changed();
        RepairEnd {
            again: self.repair_again,
            find_homes: ok,
        }
    }

    /// Asks for marked messages whose channel is unknown, once per session,
    /// so channel marks can cover them. Skipped while overrides exist.
    pub fn homes_request(&mut self, reads: &dyn ReadLookup) -> Option<HomesRequest> {
        self.ensure_index();
        let state = reads.state();
        if !state.overrides.is_empty() {
            return None;
        }
        let broadest = state
            .frontiers
            .iter()
            .filter(|(key, _)| !key.contains(':'))
            .map(|(_, value)| i64::from(*value))
            .max()
            .unwrap_or(-1);
        let index = self.index.as_ref().expect("indexed");
        let mut ids: Vec<String> = Vec::new();
        for (key, value) in &state.frontiers {
            if let Some(id) = mark_message(key)
                && is_hex64(id)
                && i64::from(*value) <= broadest
                && !self.asked_homes.contains(id)
                && !index.by_id.contains_key(id)
                && reads.home(id).is_none()
                && !ids.iter().any(|known| known == id)
            {
                ids.push(id.to_owned());
            }
        }
        if ids.is_empty() {
            return None;
        }
        self.asked_homes.extend(ids.iter().cloned());
        let filters = ids
            .chunks(HOMES_BATCH)
            .map(|batch| {
                filter(json!({ "ids": batch, "kinds": CONTENT_KINDS, "limit": batch.len() }))
            })
            .collect();
        Some(HomesRequest {
            epoch: self.epoch,
            ids,
            filters,
        })
    }

    /// The channels found for a [`HomesRequest`]; pass them to
    /// [`ReadStateSync::learn_homes`]. `None` if access changed meanwhile.
    pub fn homes_found(
        &mut self,
        request: HomesRequest,
        events: &[Verified],
    ) -> Option<BTreeMap<String, String>> {
        if request.epoch != self.epoch {
            for id in &request.ids {
                self.asked_homes.remove(id);
            }
            return None;
        }
        let wanted: HashSet<&String> = request.ids.iter().collect();
        let found: BTreeMap<String, String> = events
            .iter()
            .filter(|event| is_content(event.kind_u16()) && wanted.contains(&event.id_hex()))
            .filter_map(|event| Some((event.id_hex(), event.channel()?.to_owned())))
            .collect();
        (!found.is_empty()).then_some(found)
    }

    // -- conversation lookups (§5i.8) ---------------------------------------

    /// Whether lookups are waiting to run.
    pub fn lookups_pending(&self) -> bool {
        self.queued.len() > 0
    }

    /// Takes up to 50 queued parents of the first queued channel.
    pub fn next_lookup_batch(&mut self) -> Option<LookupBatch> {
        let channel_id = self.queued.iter().next()?.1.channel_id.clone();
        let keys: Vec<String> = self
            .queued
            .iter()
            .filter(|(_, item)| item.channel_id == channel_id)
            .take(LOOKUP_CHUNK)
            .map(|(key, _)| key.clone())
            .collect();
        let items: Vec<(String, Queued)> = keys
            .into_iter()
            .filter_map(|key| self.queued.remove(&key).map(|item| (key, item)))
            .collect();
        let mut ids: Vec<String> = Vec::new();
        for (_, item) in &items {
            for id in &item.ids {
                if self.structural(id).is_none() && !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
        }
        let ids_filter = (!ids.is_empty()).then(|| {
            filter(json!({
                "kinds": CONTENT_KINDS, "ids": ids, "include_aux": true, "limit": ids.len(),
            }))
        });
        let reply_targets = items
            .iter()
            .filter(|(_, item)| !item.structural_only)
            .map(|(_, item)| ReplyTarget {
                parent_id: item.parent_id.clone(),
                root_id: item.root_id.clone(),
            })
            .collect();
        Some(LookupBatch {
            epoch: self.epoch,
            items,
            channel_id,
            ids_filter,
            reply_targets,
        })
    }

    /// The viewer's replies (with deletions) under lookup targets. `#e`
    /// also matches root tags, which the whole-thread answer relies on.
    pub fn replies_filter(
        &self,
        channel_id: &str,
        targets: &[ReplyTarget],
        until: Option<u64>,
    ) -> Filter {
        let mut ids: Vec<&str> = Vec::new();
        for target in targets {
            for id in std::iter::once(target.parent_id.as_str()).chain(target.root_id.as_deref()) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        let mut filter = filter(json!({
            "kinds": CONTENT_KINDS, "authors": [self.viewer], "#h": [channel_id], "#e": ids,
            "include_aux": true, "limit": REPAIR_LIMIT,
        }));
        if let Some(until) = until {
            filter.insert("until".into(), json!(until));
        }
        filter
    }

    fn requeue(&mut self, items: Vec<(String, Queued)>) {
        for (key, item) in items {
            if self.lookups.get(&key).is_some_and(|lookup| !lookup.done) {
                self.queued.insert(key, item);
            }
        }
    }

    /// Records a finished lookup. Fetched events are structure only: they
    /// never count and never start another lookup. Membership holds iff
    /// the viewer authored the parent or root (live) or a deciding reply.
    pub fn complete_lookup(
        &mut self,
        batch: LookupBatch,
        fetched: &[Verified],
        replies: &[Verified],
    ) {
        self.lookup_failures = 0;
        if batch.epoch != self.epoch {
            self.requeue(batch.items);
            return;
        }
        let channel_id = batch.channel_id;
        if !self.allowed(&channel_id) {
            return;
        }
        let removed: HashSet<String> = fetched
            .iter()
            .chain(replies)
            .filter(|event| matches!(event.kind_u16(), kinds::DELETION | kinds::MODERATOR_DELETE))
            .flat_map(|event| {
                let author = event.pubkey_hex();
                e_targets(event)
                    .into_iter()
                    .map(move |id| format!("{author}:{id}"))
                    .collect::<Vec<_>>()
            })
            .collect();
        let viewer = self.viewer.clone();
        let live = |unread: &Self, event: &Verified| {
            let author = event.pubkey_hex();
            is_content(event.kind_u16())
                && event.channel() == Some(channel_id.as_str())
                && !removed.contains(&format!("{author}:{}", event.id_hex()))
                && !(author == viewer && unread.retracted.contains(&event.id_hex()))
        };
        for event in fetched {
            if live(self, event) {
                self.lookup_events.insert(event.id_hex(), event.clone());
                while self.lookup_events.len() > LOOKUP_EVENTS {
                    self.lookup_events.pop_oldest();
                }
            }
        }
        let mine: Vec<&Verified> = replies
            .iter()
            .filter(|event| event.pubkey_hex() == viewer && live(self, event))
            .collect();
        for (key, item) in batch.items {
            if item.structural_only {
                self.remember(
                    key,
                    Lookup {
                        channel_id: channel_id.clone(),
                        done: true,
                        evidence: None,
                        more: false,
                    },
                );
                continue;
            }
            let target = ReplyTarget {
                parent_id: item.parent_id.clone(),
                root_id: item.root_id.clone(),
            };
            let found: Vec<&Verified> = mine
                .iter()
                .copied()
                .filter(|event| decides(event, &target))
                .collect();
            let authored: Vec<Verified> = std::iter::once(&item.parent_id)
                .chain(item.root_id.as_ref())
                .filter_map(|id| self.structural(id))
                .filter(|event| event.pubkey_hex() == viewer && live(self, event))
                .cloned()
                .collect();
            self.remember(
                key.clone(),
                Lookup {
                    channel_id: channel_id.clone(),
                    done: true,
                    evidence: None,
                    more: found.len() + authored.len() > 1,
                },
            );
            let newest = found
                .iter()
                .copied()
                .fold(None::<&Verified>, |best, event| match best {
                    Some(best) if event.created_at <= best.created_at => Some(best),
                    _ => Some(event),
                });
            if let Some(chosen) = authored.first().cloned().or_else(|| newest.cloned()) {
                self.witness(&key, chosen);
            }
        }
        self.reindex();
    }

    fn remember(&mut self, key: String, lookup: Lookup) {
        self.lookups.insert(key, lookup);
        while self.lookups.len() > LOOKUPS {
            self.lookups.pop_oldest();
        }
    }

    /// A lookup read failed: its parents stay pending. Returns the backoff
    /// before the next attempt (`min(60 s, 1 s · 2^failures)`), or `None`
    /// when a reset retired the batch and the queue may run again now.
    pub fn fail_lookup(&mut self, batch: LookupBatch) -> Option<u64> {
        let stale = batch.epoch != self.epoch;
        self.requeue(batch.items);
        if stale {
            return None;
        }
        let delay = 1000u64
            .saturating_mul(1 << self.lookup_failures.min(16))
            .min(60_000);
        self.lookup_failures += 1;
        Some(delay)
    }

    // -- follows ------------------------------------------------------------

    /// Whether the viewer follows a root: an explicit choice, else having
    /// written or replied anywhere in its thread.
    pub fn following(&mut self, channel_id: &str, root_id: &str) -> bool {
        if !self.allowed(channel_id) {
            return false;
        }
        let key = conversation_key(channel_id, &root_id.to_ascii_lowercase());
        if let Some(choice) = self.choices.get(&key) {
            return *choice;
        }
        self.ensure_index();
        self.index
            .as_ref()
            .is_some_and(|index| index.own_threads.contains(&key))
    }

    /// Saves an explicit choice on this device; unchanged if not saved.
    pub fn follow(&mut self, channel_id: &str, root_id: &str, following: bool) -> Result<()> {
        let valid_root = root_id.len() == 64 && root_id.bytes().all(|b| b.is_ascii_hexdigit());
        if !self.allowed(channel_id) || !valid_root {
            return Err(UnreadError::ThreadUnavailable);
        }
        let mut next = self.choices.clone();
        next.insert(
            conversation_key(channel_id, &root_id.to_ascii_lowercase()),
            following,
        );
        while next.len() > THREAD_FOLLOW_LIMIT {
            next.pop_oldest();
        }
        let saved: Vec<(String, bool)> = next
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect();
        self.follows.write(&saved).map_err(UnreadError::Follows)?;
        self.choices = next;
        self.changed();
        Ok(())
    }

    // -- marking read -------------------------------------------------------

    fn deleted(&mut self, event: &Verified) -> bool {
        self.ensure_index();
        self.index
            .as_ref()
            .is_some_and(|index| index.tombstones.contains(&event.id_hex()))
    }

    fn require_message(&mut self, target: &ReadTarget, id: &str) -> Result<Verified> {
        target.key()?;
        let channel_id = target.channel_id();
        let event = self
            .events
            .get(id)
            .filter(|event| is_content(event.kind_u16()) && event.channel() == Some(channel_id))
            .cloned();
        let Some(event) = event.filter(|_| self.allowed(channel_id)) else {
            return Err(UnreadError::MessageUnavailable);
        };
        if self.deleted(&event) {
            return Err(UnreadError::MessageUnavailable);
        }
        match target {
            ReadTarget::Thread { root_id, .. } if self.root(&event).as_ref() != Some(root_id) => {
                return Err(UnreadError::NotInTarget);
            }
            ReadTarget::Message { message_id, .. } if message_id != id => {
                return Err(UnreadError::NotInTarget);
            }
            ReadTarget::Channel { .. }
                if thread_reference(&event).is_some() && !is_broadcast(&event) =>
            {
                return Err(UnreadError::ReplyAdvancesChannel);
            }
            _ => {}
        }
        Ok(event)
    }

    /// The message and its loaded reply subtree.
    fn message_subtree(&mut self, channel_id: &str, message_id: &str) -> Result<Vec<Verified>> {
        let selected =
            self.require_message(&ReadTarget::message(channel_id, message_id), message_id)?;
        let index = self.index.as_ref().expect("indexed");
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for entry in index.channel(channel_id) {
            let Some(parent) = thread_reference(&entry.event).map(|r| r.parent_id) else {
                continue;
            };
            if entry.event.id == selected.id || self.root(&entry.event).is_none() {
                continue;
            }
            children
                .entry(parent)
                .or_default()
                .push(entry.event.id_hex());
        }
        let mut ids = vec![message_id.to_owned()];
        let mut seen: HashSet<&str> = HashSet::from([message_id]);
        let mut stack = vec![message_id.to_owned()];
        while let Some(parent) = stack.pop() {
            for id in children.get(&parent).into_iter().flatten() {
                if seen.insert(id) {
                    ids.push(id.clone());
                    stack.push(id.clone());
                }
            }
        }
        Ok(ids
            .iter()
            .filter_map(|id| self.events.get(id).cloned())
            .collect())
    }

    fn apply<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        update: ReadUpdate,
        now_ms: u64,
    ) -> Result<ReadMutationResult> {
        self.ensure_index();
        let coverage = UnreadCoverage { unread: &*self };
        Ok(reads.apply(update, Some(&coverage), now_ms)?)
    }

    /// Explicit prefix read through one verified message of the target.
    /// A channel target rejects non-broadcast replies.
    pub fn mark_through<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        target: &ReadTarget,
        message_id: &str,
        now_ms: u64,
    ) -> Result<ReadMutationResult> {
        let event = self.require_message(target, message_id)?;
        let key = target.key()?;
        self.apply(
            reads,
            ReadUpdate::read(key, timestamp(&event)?, Vec::new()),
            now_ms,
        )
    }

    fn channel_token(&mut self, channel_id: &str) -> u64 {
        if let Some(token) = self.channel_tokens.get(channel_id) {
            return *token;
        }
        self.next_token += 1;
        self.channel_tokens
            .insert(channel_id.to_owned(), self.next_token);
        self.next_token
    }

    fn token_valid(&self, channel_id: &str, token: u64) -> bool {
        self.channel_tokens.get(channel_id) == Some(&token) && self.allowed(channel_id)
    }

    /// Captures an explicit channel read at the click: the cut is
    /// `max(clicked_at, newest retained message incl. replies)`, and the
    /// local marks it clears are the channel's, its retained messages',
    /// their reply roots' and its retained top-level threads'. Applying it
    /// again retries the same cut.
    pub fn prepare_channel_read(
        &mut self,
        channel_id: &str,
        clicked_at: u64,
    ) -> Result<ChannelReadIntent> {
        if !self.allowed(channel_id) {
            return Err(UnreadError::TargetUnavailable);
        }
        self.ensure_index();
        let index = self.index.as_ref().expect("indexed");
        let mut cut = clicked_at;
        let mut keys = vec![channel_id.to_owned(), message_force_key(channel_id)];
        let mut seen: HashSet<String> = keys.iter().cloned().collect();
        let mut has_rows = false;
        let mut add = |key: String| {
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        };
        for entry in index.channel(channel_id) {
            has_rows = true;
            cut = cut.max(entry.event.created_at_secs());
            add(format!("msg:{}", entry.event.id_hex()));
            if let Some(root) = &entry.root_id {
                add(format!("thread:{root}"));
            }
            if thread_reference(&entry.event).is_none() {
                add(format!("thread:{}", entry.event.id_hex()));
            }
        }
        let cut = u32::try_from(cut).map_err(|_| ReadStateError::InvalidFrontier)?;
        let token = self.channel_token(channel_id);
        Ok(ChannelReadIntent {
            channel_id: channel_id.to_owned(),
            cut,
            keys,
            token,
            has_rows,
        })
    }

    /// Applies a prepared channel read in one save. Without evidence on a
    /// host that cannot sync, it only clears local marks.
    pub fn apply_channel_read<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        intent: &ChannelReadIntent,
        now_ms: u64,
    ) -> Result<ReadMutationResult> {
        let channel_id = intent.channel_id.as_str();
        if !self.token_valid(channel_id, intent.token) {
            return Err(ReadStateError::Expired.into());
        }
        let update = if intent.has_rows || reads.capability() == ReadCapability::FrontierSync {
            ReadUpdate::read(channel_id, intent.cut, intent.keys.clone())
        } else {
            ReadUpdate::clear_local(channel_id, intent.keys.clone())
        };
        let result = self.apply(reads, update, now_ms)?;
        if self.forced.remove(channel_id).is_some() {
            self.changed();
        }
        Ok(result)
    }

    pub fn mark_channel_read<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        channel_id: &str,
        clicked_at: u64,
        now_ms: u64,
    ) -> Result<ReadMutationResult> {
        let intent = self.prepare_channel_read(channel_id, clicked_at)?;
        self.apply_channel_read(reads, &intent, now_ms)
    }

    /// Channel reads over every accessible channel that still has manual
    /// intent, an unread row or a `thread:` local mark, with one click time.
    /// Already-read channels cost nothing. A failure does not stop the
    /// sweep; the first is returned after it. Each result names its
    /// channel.
    pub fn mark_all_channels_read<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        clicked_at: u64,
        now_ms: u64,
    ) -> Result<Vec<(String, ReadMutationResult)>> {
        self.ensure_index();
        let ids: Vec<String> = self
            .channels
            .iter()
            .filter(|channel| channel.allowed())
            .map(|channel| channel.id.clone())
            .collect();
        let mut pending = Vec::new();
        for channel_id in ids {
            let (snapshot, _) = self.compute(&ReadTarget::channel(&channel_id), reads);
            let index = self.index.as_ref().expect("indexed");
            let dm = self.is_dm(&channel_id);
            let any = index.channel(&channel_id).any(|entry| {
                self.is_unread(index, entry, reads, dm)
                    || entry
                        .root_id
                        .as_ref()
                        .is_some_and(|root| reads.local_unread(&format!("thread:{root}")).is_some())
                    || (thread_reference(&entry.event).is_none()
                        && reads
                            .local_unread(&format!("thread:{}", entry.event.id_hex()))
                            .is_some())
            });
            if snapshot.manual != Manual::None || any {
                pending.push(self.prepare_channel_read(&channel_id, clicked_at)?);
            }
        }
        let mut results = Vec::new();
        let mut failure = None;
        for intent in pending {
            if !self.allowed(&intent.channel_id) {
                continue;
            }
            match self.apply_channel_read(reads, &intent, now_ms) {
                Ok(result) => results.push((intent.channel_id.clone(), result)),
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(results),
        }
    }

    /// Reads a message and its loaded reply subtree (not the viewer's own)
    /// as `msg:` marks, atomically, and ends their forced unread.
    pub fn mark_message_read<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        channel_id: &str,
        message_id: &str,
        now_ms: u64,
    ) -> Result<ReadMutationResult> {
        let rows = self.message_subtree(channel_id, message_id)?;
        let ids: HashSet<String> = rows.iter().map(|row| row.id_hex()).collect();
        let remaining = self
            .forced
            .get(channel_id)
            .is_some_and(|forced| forced.iter().any(|id| !ids.contains(id)));
        let mut messages = Vec::new();
        for row in rows.iter().filter(|row| row.pubkey_hex() != self.viewer) {
            let root_id = thread_reference(row).and_then(|_| self.root(row));
            messages.push(MessageRead {
                key: format!("msg:{}", row.id_hex()),
                timestamp: timestamp(row)?,
                channel_id: channel_id.to_owned(),
                root_id,
            });
        }
        let clear_force = (!remaining).then(|| message_force_key(channel_id));
        let result = self.apply(reads, ReadUpdate::messages(messages, clear_force), now_ms)?;
        if let Some(forced) = self.forced.get_mut(channel_id) {
            forced.retain(|id| !ids.contains(id));
            if forced.is_empty() {
                self.forced.remove(channel_id);
            }
        }
        self.changed();
        Ok(result)
    }

    /// Forces a message and its loaded reply subtree unread for this
    /// channel visit, leaving a device-local sidebar hint
    /// (`message-force:<channel>`) until the channel is next opened.
    pub fn mark_message_unread<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        channel_id: &str,
        message_id: &str,
        now_ms: u64,
    ) -> Result<ReadMutationResult> {
        let rows = self.message_subtree(channel_id, message_id)?;
        let result = self.apply(
            reads,
            ReadUpdate::mark_local(message_force_key(channel_id)),
            now_ms,
        )?;
        self.forced
            .entry(channel_id.to_owned())
            .or_default()
            .extend(rows.iter().map(|row| row.id_hex()));
        self.changed();
        Ok(result)
    }

    fn check_local_target(&mut self, target: &ReadTarget) -> Result<String> {
        let key = target.key()?;
        let channel_id = target.channel_id().to_owned();
        if !self.allowed(&channel_id) {
            return Err(UnreadError::UnreadTargetUnavailable);
        }
        self.channel_token(&channel_id);
        match target {
            ReadTarget::Channel { .. } => {}
            ReadTarget::Thread { root_id: id, .. } | ReadTarget::Message { message_id: id, .. } => {
                let id = id.clone();
                self.require_message(target, &id)
                    .map_err(|_| UnreadError::UnreadTargetUnavailable)?;
            }
        }
        Ok(key)
    }

    /// Marks a target unread on this device only (never synced).
    pub fn mark_unread_local<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        target: &ReadTarget,
        now_ms: u64,
    ) -> Result<ReadMutationResult> {
        let key = self.check_local_target(target)?;
        self.apply(reads, ReadUpdate::mark_local(key), now_ms)
    }

    pub fn clear_unread_local<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        target: &ReadTarget,
        now_ms: u64,
    ) -> Result<ReadMutationResult> {
        let key = self.check_local_target(target)?;
        self.apply(
            reads,
            ReadUpdate::clear_local(key.clone(), vec![key]),
            now_ms,
        )
    }

    /// Opening a channel reconciles the previous visit's sidebar hint.
    pub fn enter_channel<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        channel_id: &str,
        now_ms: u64,
    ) -> Result<()> {
        if self.entered.contains(channel_id) {
            return Ok(());
        }
        if !self.allowed(channel_id) {
            return Err(UnreadError::VisitExpired);
        }
        let force = message_force_key(channel_id);
        if reads.local_unread(&force).is_some() {
            self.apply(
                reads,
                ReadUpdate::clear_local(force.clone(), vec![force]),
                now_ms,
            )?;
        }
        self.entered.insert(channel_id.to_owned());
        Ok(())
    }

    /// Ends the visit; forced rows drop, the sidebar hint stays.
    pub fn leave_channel(&mut self, channel_id: &str) {
        if self.forced.remove(channel_id).is_some() {
            self.changed();
        }
        self.entered.remove(channel_id);
    }

    // -- reading leases (§5i.10) --------------------------------------------

    /// Starts a reading lease (≤64 at once).
    pub fn reading(&mut self, channel_id: &str, reads: &dyn ReadLookup) -> Result<ReadingLease> {
        if !self.allowed(channel_id) || self.leases.len() >= READING_LEASES {
            return Err(UnreadError::ReadingUnavailable);
        }
        self.next_lease += 1;
        let lease = ReadingLease {
            id: self.next_lease,
            channel_id: channel_id.to_owned(),
            epoch: self.epoch,
            manual_revision: reads.revision(),
            observed: HashSet::new(),
            dm_read: false,
        };
        self.leases.insert(
            lease.id,
            LeaseView {
                channel_id: lease.channel_id.clone(),
                epoch: lease.epoch,
                manual_revision: lease.manual_revision,
                ids: HashSet::new(),
            },
        );
        Ok(lease)
    }

    pub fn end_reading(&mut self, lease: ReadingLease) {
        self.leases.remove(&lease.id);
    }

    fn view_valid(&self, view: &LeaseView, reads: &dyn ReadLookup) -> bool {
        view.epoch == self.epoch
            && self.allowed(&view.channel_id)
            && reads.local_unread(&view.channel_id).unwrap_or(0) <= view.manual_revision
    }

    fn lease_valid(&self, lease: &ReadingLease, reads: &dyn ReadLookup) -> bool {
        self.leases
            .get(&lease.id)
            .is_some_and(|view| self.view_valid(view, reads))
    }

    /// The rows currently fully visible (≤128). Publishes no read intent;
    /// pass an empty list while the surface is hidden.
    pub fn view(&mut self, lease: &ReadingLease, ids: &[String], reads: &dyn ReadLookup) {
        if !self.lease_valid(lease, reads) || ids.len() > VIEW_IDS {
            return;
        }
        let verified: HashSet<String> = ids
            .iter()
            .filter(|id| {
                self.require_message(&ReadTarget::message(&lease.channel_id, id.as_str()), id)
                    .is_ok()
            })
            .cloned()
            .collect();
        if let Some(view) = self.leases.get_mut(&lease.id) {
            view.ids = verified;
        }
    }

    fn update_fence(lease: &ReadingLease, update: ReadUpdate) -> ReadUpdate {
        update.fenced(lease.channel_id.clone(), lease.manual_revision)
    }

    /// Reading a DM reads all of it, through the newest retained message,
    /// once per lease. `None` outside DMs.
    fn read_dm<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        lease: &mut ReadingLease,
        witnesses: &[(ReadTarget, String)],
        now_ms: u64,
    ) -> Option<Result<()>> {
        if !self.is_dm(&lease.channel_id) {
            return None;
        }
        if lease.dm_read {
            return Some(Ok(()));
        }
        for (target, id) in witnesses {
            if self.require_message(target, id).is_err() {
                continue;
            }
            lease.dm_read = true;
            let index = self.index.as_ref().expect("indexed");
            let cut = index
                .channel(&lease.channel_id)
                .map(|entry| entry.event.created_at_secs())
                .max()
                .unwrap_or(0);
            let behind = reads
                .state()
                .frontier(&lease.channel_id)
                .map_or(-1, i64::from)
                < cut as i64;
            if !behind && reads.local_unread(&lease.channel_id).is_none() {
                return Some(Ok(()));
            }
            let Ok(cut) = u32::try_from(cut) else {
                return Some(Err(ReadStateError::InvalidFrontier.into()));
            };
            let channel = lease.channel_id.clone();
            let update =
                Self::update_fence(lease, ReadUpdate::read(channel.clone(), cut, vec![channel]));
            return Some(self.apply(reads, update, now_ms).map(|_| ()));
        }
        Some(Ok(()))
    }

    /// Bottom dwell: catch up the channel (`activity:<c>`) or a thread
    /// (`thread-activity:<root>`) through a verified message, unless newer
    /// retained activity exists. Channel catch-up never reads mentions,
    /// DMs, broadcasts or replies, and ends a manual channel unread.
    pub fn catch_up<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        lease: &mut ReadingLease,
        message_id: &str,
        root_id: Option<&str>,
        now_ms: u64,
    ) -> Result<()> {
        if !self.lease_valid(lease, reads) {
            return Ok(());
        }
        let channel_id = lease.channel_id.clone();
        let target = match root_id {
            Some(root) => ReadTarget::thread(&channel_id, root),
            None => ReadTarget::channel(&channel_id),
        };
        self.ensure_index();
        if let Some(result) = self.read_dm(
            reads,
            lease,
            &[(target.clone(), message_id.to_owned())],
            now_ms,
        ) {
            return result;
        }
        let event = self.require_message(&target, message_id)?;
        let key = match root_id {
            Some(root) => format!("thread-activity:{root}"),
            None => format!("activity:{channel_id}"),
        };
        let index = self.index.as_ref().expect("indexed");
        let created = event.created_at_secs();
        // A historical window is not the live bottom.
        let newer = index.channel(&channel_id).any(|entry| {
            entry.event.created_at_secs() > created
                && match root_id {
                    Some(root) => entry.root_id.as_deref() == Some(root),
                    None => thread_reference(&entry.event).is_none() || entry.broadcast,
                }
        });
        if newer {
            return Ok(());
        }
        // Replies can be newer than the last top-level row; quiet those too
        // without acknowledging the replies themselves.
        let cut = match root_id {
            Some(_) => created,
            None => index
                .channel(&channel_id)
                .map(|entry| entry.event.created_at_secs())
                .fold(created, u64::max),
        };
        let cut = u32::try_from(cut).map_err(|_| ReadStateError::InvalidFrontier)?;
        let state = reads.state();
        let covered = [
            state.frontier(&key),
            state.frontier(&channel_id),
            root_id.and_then(|root| state.frontier(&format!("thread:{root}"))),
        ]
        .into_iter()
        .flatten()
        .any(|frontier| frontier >= cut);
        let end_manual = root_id.is_none() && reads.local_unread(&channel_id).is_some();
        let fence = |update: ReadUpdate| {
            let update = Self::update_fence(lease, update);
            match root_id {
                Some(root) => update.fenced(format!("thread:{root}"), lease.manual_revision),
                None => update,
            }
        };
        let update = if covered {
            if !end_manual {
                return Ok(());
            }
            fence(ReadUpdate::clear_local(
                channel_id.clone(),
                vec![channel_id],
            ))
        } else if end_manual {
            fence(ReadUpdate::read(key, cut, vec![channel_id]))
        } else {
            fence(ReadUpdate::observe(key, cut))
        };
        self.apply(reads, update, now_ms).map(|_| ())
    }

    /// Visible-row dwell: an individual `msg:` mark per verified message
    /// not already read. Does not clear local manual unread.
    pub fn observe<J: ReadStateJournal>(
        &mut self,
        reads: &mut ReadStateSync<J>,
        lease: &mut ReadingLease,
        ids: &[String],
        now_ms: u64,
    ) -> Result<()> {
        if !self.lease_valid(lease, reads) || ids.len() > VIEW_IDS {
            return Ok(());
        }
        let channel_id = lease.channel_id.clone();
        self.ensure_index();
        let witnesses: Vec<(ReadTarget, String)> = ids
            .iter()
            .map(|id| (ReadTarget::message(&channel_id, id.as_str()), id.clone()))
            .collect();
        if let Some(result) = self.read_dm(reads, lease, &witnesses, now_ms) {
            return result;
        }
        for (target, id) in witnesses {
            if !self.lease_valid(lease, reads) || lease.observed.contains(&id) {
                continue;
            }
            let event = self.require_message(&target, &id)?;
            let key = target.key()?;
            let root = thread_reference(&event).and_then(|_| self.root(&event));
            let index = self.index.as_ref().expect("indexed");
            let state = reads.state();
            let caught_up = index.entry(&id).is_some_and(|entry| {
                !state.overrides.contains_key(&key)
                    && self.caught_up(index, entry, &|key| state.frontier(key))
            });
            let read = effective_frontier(state, &key, Some(&channel_id), root.as_deref())
                .is_some_and(|frontier| u64::from(frontier) >= event.created_at_secs());
            if !caught_up && !read {
                let update =
                    Self::update_fence(lease, ReadUpdate::observe(key, timestamp(&event)?));
                self.apply(reads, update, now_ms)?;
            }
            lease.observed.insert(id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Verifier;
    use crate::read_state::{MemoryJournal, ReadJournal, sign_read_state};
    use buzz_identity::{Identity, Purpose};
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};

    const T: u64 = 100_000;

    #[test]
    fn previews_text_and_diffs() {
        let event = |kind: u16, content: &str, list: &[&[&str]]| {
            EventBuilder::new(Kind::from_u16(kind), content)
                .tags(
                    list.iter()
                        .map(|tag| Tag::parse(tag.iter().copied()).unwrap()),
                )
                .finalize(&Keys::generate())
                .unwrap()
        };
        assert_eq!(latest_preview(&event(kinds::CHAT, "hi", &[])), "hi");
        assert_eq!(
            latest_preview(&event(kinds::AGENT_MESSAGE, r#"{"content":"done"}"#, &[])),
            "done"
        );
        let diff = event(
            kinds::DIFF,
            "@@",
            &[&["file", "src/a.rs"], &["description", "Fix"]],
        );
        assert_eq!(latest_preview(&diff), "Diff: src/a.rs — Fix");
        assert_eq!(
            latest_preview(&event(kinds::DIFF, "@@", &[])),
            "Diff: patch"
        );
        // Attached images leave the text, as rows show it.
        let url = "https://x.test/a.png";
        let image = event(
            kinds::CHAT,
            &format!("look ![a.png]({url})"),
            &[&["imeta", &format!("url {url}"), "m image/png"]],
        );
        assert_eq!(latest_preview(&image).trim(), "look");
    }

    fn tags(list: &[&[&str]]) -> Vec<Vec<String>> {
        list.iter()
            .map(|tag| tag.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    fn reply_tags(root: &str, parent: &str) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        if root != parent {
            out.push(vec!["e".into(), root.into(), String::new(), "root".into()]);
        }
        out.push(vec![
            "e".into(),
            parent.into(),
            String::new(),
            "reply".into(),
        ]);
        out
    }

    struct H {
        viewer: Identity,
        alice: Keys,
        reads: ReadStateSync<MemoryJournal>,
        unread: Unread,
        verifier: Verifier,
    }

    impl H {
        fn new() -> Self {
            let viewer = Identity::ephemeral();
            let reads = ReadStateSync::open(
                viewer.pubkey_hex(),
                ReadCapability::FrontierSync,
                MemoryJournal::default(),
            );
            let unread = Unread::new(UnreadOptions {
                viewer: viewer.pubkey_hex(),
                relay_author: Some(Keys::generate().public_key().to_hex()),
                authority: None,
            });
            Self {
                viewer,
                alice: Keys::generate(),
                reads,
                unread,
                verifier: Verifier::default(),
            }
        }

        fn grant(&mut self, channels: &[(&str, Option<&str>)]) {
            let list = channels
                .iter()
                .map(|(id, kind)| ChannelInfo::joined(*id, *kind))
                .collect();
            self.unread.set_channels(list);
        }

        fn vpk(&self) -> String {
            self.viewer.pubkey_hex()
        }

        fn mine(&self, channel: &str, content: &str, at: u64, extra: Vec<Vec<String>>) -> Verified {
            let mut all = vec![vec!["h".to_string(), channel.to_string()]];
            all.extend(extra);
            let unsigned = self
                .viewer
                .template(Purpose::Content, 9, at, all, content.into())
                .unwrap();
            let event = self.viewer.sign(Purpose::Content, unsigned).unwrap();
            self.verifier.verify(event).unwrap()
        }

        fn by(
            &self,
            keys: &Keys,
            kind: u16,
            channel: Option<&str>,
            content: &str,
            at: u64,
            extra: Vec<Vec<String>>,
        ) -> Verified {
            let mut all: Vec<Vec<String>> = channel
                .map(|channel| vec!["h".to_string(), channel.to_string()])
                .into_iter()
                .collect();
            all.extend(extra);
            let event = EventBuilder::new(Kind::from_u16(kind), content)
                .tags(all.into_iter().map(|tag| Tag::parse(tag).unwrap()))
                .custom_created_at(Timestamp::from_secs(at))
                .finalize(keys)
                .unwrap();
            self.verifier.verify(event).unwrap()
        }

        fn alice(
            &self,
            channel: &str,
            content: &str,
            at: u64,
            extra: Vec<Vec<String>>,
        ) -> Verified {
            self.by(&self.alice, 9, Some(channel), content, at, extra)
        }

        fn emit(&mut self, events: &[Verified]) {
            assert!(self.unread.accept(events));
        }

        fn snapshot(&mut self, target: &ReadTarget) -> Arc<UnreadSnapshot> {
            self.unread.snapshot(target, &self.reads)
        }

        fn room(&mut self) -> Arc<UnreadSnapshot> {
            self.snapshot(&ReadTarget::channel("room"))
        }

        fn counts(&mut self, target: &ReadTarget) -> (Option<u32>, Option<u32>) {
            let snapshot = self.snapshot(target);
            (snapshot.observed_count, snapshot.attention_count)
        }

        fn attention(&mut self, channel: &str, id: &Verified) -> MessageAttention {
            self.unread.attention(channel, &id.id_hex(), &self.reads)
        }

        fn frontiers(&self) -> BTreeMap<String, u32> {
            self.reads.journal().unwrap().state.frontiers.clone()
        }

        /// Finishes every queued conversation lookup with no results.
        fn settle(&mut self) {
            while let Some(batch) = self.unread.next_lookup_batch() {
                self.unread.complete_lookup(batch, &[], &[]);
            }
        }
    }

    fn room() -> ReadTarget {
        ReadTarget::channel("room")
    }

    #[test]
    fn direct_messages_are_never_mentions() {
        let mut h = H::new();
        h.grant(&[("dm", Some("dm"))]);
        let viewer = h.vpk();
        let direct = h.alice("dm", "hi", 10, tags(&[&["p", &viewer]]));
        h.emit(&[direct]);
        let snapshot = h.snapshot(&ReadTarget::channel("dm"));
        assert_eq!(snapshot.observed_count, Some(1));
        assert_eq!(snapshot.mention_count, Some(0), "direct wins in DMs");
    }

    #[test]
    fn thread_activity_previews_show_the_edited_text() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let root = h.mine("room", "root", 10, vec![]);
        let reply = h.alice(
            "room",
            "first draft",
            11,
            reply_tags(&root.id_hex(), &root.id_hex()),
        );
        let edit = h.by(
            &h.alice.clone(),
            kinds::EDIT,
            Some("room"),
            "final words",
            12,
            tags(&[&["e", &reply.id_hex()]]),
        );
        h.emit(&[root, reply, edit]);
        let activity = h.unread.activity("room", &h.reads);
        let items = activity.items.as_ref().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].preview, "final words");
    }

    #[test]
    fn activity_previews_apply_edits_by_the_attributed_author() {
        let mut h = H::new();
        let relay = Keys::generate();
        h.unread = Unread::new(UnreadOptions {
            viewer: h.vpk(),
            relay_author: Some(relay.public_key().to_hex()),
            authority: Some(relay.public_key().to_hex()),
        });
        h.grant(&[("room", Some("stream"))]);
        let root = h.mine("room", "root", 10, vec![]);
        let mut extra = reply_tags(&root.id_hex(), &root.id_hex());
        extra.push(vec!["actor".into(), h.alice.public_key().to_hex()]);
        let reply = h.by(&relay, kinds::CHAT, Some("room"), "first draft", 11, extra);
        let edit = h.by(
            &h.alice.clone(),
            kinds::EDIT,
            Some("room"),
            "final words",
            12,
            tags(&[&["e", &reply.id_hex()]]),
        );
        h.emit(&[root, reply, edit]);
        let activity = h.unread.activity("room", &h.reads);
        assert_eq!(activity.items.as_ref().unwrap()[0].preview, "final words");
    }

    #[test]
    fn channel_previews_show_the_edited_timeline_message() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let top = h.alice("room", "first draft", 10, vec![]);
        let edit = h.by(
            &h.alice.clone(),
            kinds::EDIT,
            Some("room"),
            "final words",
            11,
            tags(&[&["e", &top.id_hex()]]),
        );
        let reply = h.alice(
            "room",
            "in a thread",
            12,
            reply_tags(&top.id_hex(), &top.id_hex()),
        );
        h.emit(&[top, edit, reply.clone()]);
        let latest = h.room().latest_message.clone().unwrap();
        assert_eq!(latest.id, reply.id_hex(), "replies are still evidence");
        assert_eq!(latest.preview, "final words");
    }

    #[test]
    fn mentions_broadcasts_and_followed_replies_are_attention() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let root = h.mine("room", "root", 10, vec![]);
        let ordinary = h.alice("room", "ordinary", 11, vec![]);
        h.emit(&[root.clone(), ordinary]);
        assert_eq!(h.counts(&room()), (Some(1), Some(0)));

        let viewer = h.vpk();
        let missing = "f".repeat(64);
        let mentioned = h.alice("room", "mentioned", 12, tags(&[&["p", &viewer]]));
        let mut broadcast_tags = reply_tags(&root.id_hex(), &root.id_hex());
        broadcast_tags.push(vec!["broadcast".into(), "1".into()]);
        let broadcast = h.alice("room", "broadcast", 13, broadcast_tags);
        let participating = h.alice(
            "room",
            "reply",
            14,
            reply_tags(&root.id_hex(), &root.id_hex()),
        );
        let mut missing_tags = reply_tags(&missing, &missing);
        missing_tags.push(vec!["broadcast".into(), "1".into()]);
        let orphan = h.alice("room", "broadcast without retained root", 15, missing_tags);
        h.emit(&[mentioned.clone(), broadcast, participating, orphan.clone()]);
        assert_eq!(h.counts(&room()), (Some(5), Some(4)));
        assert_eq!(h.room().mention_count, Some(1));
        let activity = h.unread.activity("room", &h.reads);
        assert_eq!(activity.items.as_ref().unwrap().len(), 1);
        assert_eq!(activity.items.as_ref().unwrap()[0].unread_count, 2);
        let attention = h.attention("room", &orphan);
        assert_eq!(attention.status, AttentionStatus::Unknown);
        assert!(attention.unread);
        let attention = h.attention("room", &mentioned);
        assert_eq!(attention.category, Some(Category::Mention));
        assert!(!attention.muted_suppresses());
    }

    #[test]
    fn replies_count_only_in_the_viewers_conversations() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let root = h.alice("room", "root", 10, vec![]);
        let r = root.id_hex();
        let mine = h.mine("room", "my reply", 11, reply_tags(&r, &r));
        let sibling = h.alice("room", "same level as me", 12, reply_tags(&r, &r));
        let answer = h.alice("room", "answer to me", 13, reply_tags(&r, &mine.id_hex()));
        let nested = h.alice("room", "nested", 14, reply_tags(&r, &answer.id_hex()));
        let other_root = h.alice("room", "other thread", 15, vec![]);
        let o = other_root.id_hex();
        let unjoined = h.alice("room", "unjoined", 16, reply_tags(&o, &o));
        h.emit(&[
            root,
            mine,
            sibling.clone(),
            answer.clone(),
            nested.clone(),
            other_root,
            unjoined.clone(),
        ]);
        for reply in [&sibling, &answer, &nested] {
            let attention = h.attention("room", reply);
            assert_eq!(attention.category, Some(Category::Thread));
            assert!(attention.unread);
        }
        let attention = h.attention("room", &unjoined);
        assert!(!attention.unread);
        assert_eq!(attention.category, None);
        // Two top-level roots plus sibling, answer and nested.
        assert_eq!(h.counts(&room()), (Some(5), Some(3)));
        assert_eq!(h.counts(&ReadTarget::thread("room", &r)).0, Some(3));
        let activity = h.unread.activity("room", &h.reads);
        let items = activity.items.as_ref().unwrap();
        assert_eq!(
            (
                items.len(),
                items[0].root_id.as_str(),
                items[0].unread_count
            ),
            (1, r.as_str(), 3)
        );

        let viewer = h.vpk();
        let mut mention_tags = reply_tags(&o, &unjoined.id_hex());
        mention_tags.push(vec!["p".into(), viewer]);
        let mention = h.alice("room", "unjoined mention", 17, mention_tags);
        h.emit(std::slice::from_ref(&mention));
        let attention = h.attention("room", &mention);
        assert_eq!(
            (attention.category, attention.unread),
            (Some(Category::Mention), true)
        );

        // Joining the other thread makes its replies count.
        let joining = h.mine("room", "joining", 18, reply_tags(&o, &unjoined.id_hex()));
        h.emit(&[joining]);
        let attention = h.attention("room", &unjoined);
        assert_eq!(
            (attention.category, attention.unread),
            (Some(Category::Thread), true)
        );
        assert_eq!(h.counts(&room()), (Some(7), Some(5)));
    }

    #[test]
    fn only_authorized_deletions_hide_messages() {
        let mut h = H::new();
        h.grant(&[("room", None)]);
        let row = h.alice("room", "retained", 11, vec![]);
        let unsigned = h
            .viewer
            .template(
                Purpose::Content,
                5,
                12,
                tags(&[&["e", &row.id_hex()]]),
                String::new(),
            )
            .unwrap();
        let forged = h
            .verifier
            .verify(h.viewer.sign(Purpose::Content, unsigned).unwrap())
            .unwrap();
        h.emit(&[row.clone(), forged]);
        assert_eq!(h.room().observed_count, Some(1));
        let alice = h.alice.clone();
        let lease = h.unread.reading("room", &h.reads).unwrap();
        h.unread.view(&lease, &[row.id_hex()], &h.reads);
        assert!(h.attention("room", &row).viewing);
        let own = h.by(&alice, 5, None, "", 13, tags(&[&["e", &row.id_hex()]]));
        h.emit(&[own]);
        assert_eq!(h.room().observed_count, Some(0));
        let attention = h.attention("room", &row);
        assert_eq!(attention.status, AttentionStatus::Ineligible);
        assert!(!attention.viewing);
        // Access loss fails closed; a regrant does not revive the lease.
        h.grant(&[]);
        assert_eq!(h.attention("room", &row).status, AttentionStatus::Unknown);
        h.grant(&[("room", None)]);
        h.emit(std::slice::from_ref(&row));
        assert!(!h.attention("room", &row).viewing);
    }

    #[test]
    fn channel_admin_moderator_deletes_hide_messages() {
        let mut h = H::new();
        h.grant(&[("room", None), ("other", None)]);
        let row = h.alice("room", "spam", 11, vec![]);
        let admin = Keys::generate();
        let target = tags(&[&["e", &row.id_hex()]]);
        let delete = h.by(
            &admin,
            kinds::MODERATOR_DELETE,
            Some("room"),
            "",
            12,
            target,
        );
        h.emit(&[row, delete]);
        assert_eq!(h.room().observed_count, Some(1), "no admin set: parity");
        let admins: HashSet<PublicKey> = [admin.public_key()].into();
        h.unread.set_channel_admins("other", &admins);
        assert_eq!(h.room().observed_count, Some(1), "an admin elsewhere");
        h.unread.set_channel_admins("room", &admins);
        assert_eq!(h.room().observed_count, Some(0));
        h.unread.set_channel_admins("room", &HashSet::new());
        assert_eq!(h.room().observed_count, Some(1));
    }

    #[test]
    fn evidence_needs_an_accessible_channel_and_revocation_purges_it() {
        let mut h = H::new();
        h.grant(&[("room", None)]);
        let stranger = h.alice("elsewhere", "not listed", 10, vec![]);
        let row = h.alice("room", "row", 11, vec![]);
        h.emit(&[stranger, row]);
        assert_eq!(h.room().observed_count, Some(1));
        let generation = h.unread.generation();
        h.grant(&[("other", None)]);
        assert!(h.unread.generation() > generation);
        assert_eq!(h.room().observed_count, None);
        h.grant(&[("room", None), ("other", None)]);
        assert_eq!(h.room().observed_count, None);
        assert_eq!(h.room().coverage, Coverage::Unknown);
    }

    #[test]
    fn channel_read_clears_owned_marks_through_the_click() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream")), ("other", Some("stream"))]);
        let viewer = h.vpk();
        let root = h.alice("room", "root", 11, vec![]);
        let mut reply_tags_ = reply_tags(&root.id_hex(), &root.id_hex());
        reply_tags_.push(vec!["p".into(), viewer]);
        let reply = h.alice("room", "reply", 20, reply_tags_);
        let other = h.alice("other", "other", 12, vec![]);
        h.emit(&[root.clone(), reply.clone(), other]);
        let thread = ReadTarget::thread("room", root.id_hex());
        let message = ReadTarget::message("room", reply.id_hex());
        for target in [
            room(),
            thread.clone(),
            message.clone(),
            ReadTarget::channel("other"),
        ] {
            h.unread
                .mark_unread_local(&mut h.reads, &target, T)
                .unwrap();
        }
        let before = h.reads.journal().unwrap().clone();
        // The cut is the click, not the moment the save runs.
        let intent = h.unread.prepare_channel_read("room", 25).unwrap();
        let missed = h.alice("room", "missed", 22, vec![]);
        let later = h.alice("room", "later", 26, vec![]);
        h.emit(&[missed, later]);
        assert_eq!(h.room().manual, Manual::LocalOnly);
        let result = h
            .unread
            .apply_channel_read(&mut h.reads, &intent, T)
            .unwrap();
        assert_eq!(result.sync, crate::read_state::SyncKind::Pending);
        let journal = h.reads.journal().unwrap();
        assert_eq!(journal.revision, before.revision + 1);
        assert_eq!(h.frontiers(), BTreeMap::from([("room".to_string(), 25)]));
        assert_eq!(
            h.reads.journal().unwrap().local_unread,
            BTreeMap::from([("other".to_string(), before.local_unread["other"])])
        );
        for target in [&thread, &message] {
            let snapshot = h.snapshot(target);
            assert_eq!(
                (snapshot.manual, snapshot.observed_count),
                (Manual::None, Some(0))
            );
        }
        let snapshot = h.room();
        assert_eq!(
            (
                snapshot.observed_count,
                snapshot.attention_count,
                snapshot.manual
            ),
            (Some(1), Some(0), Manual::None)
        );
        let other = h.snapshot(&ReadTarget::channel("other"));
        assert_eq!(
            (other.observed_count, other.manual),
            (Some(1), Manual::LocalOnly)
        );
        // A reply cannot advance the channel frontier.
        assert_eq!(
            h.unread
                .mark_through(&mut h.reads, &room(), &reply.id_hex(), T),
            Err(UnreadError::ReplyAdvancesChannel)
        );
    }

    #[test]
    fn mark_all_reads_every_channel_with_something_to_clear() {
        let mut h = H::new();
        h.grant(&[("room", None), ("quiet", None), ("marked", None)]);
        let row = h.alice("room", "row", 11, vec![]);
        h.emit(&[row]);
        h.unread
            .mark_unread_local(&mut h.reads, &ReadTarget::channel("marked"), T)
            .unwrap();
        let results = h
            .unread
            .mark_all_channels_read(&mut h.reads, 50, T)
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            h.frontiers(),
            BTreeMap::from([("marked".to_string(), 50), ("room".to_string(), 50)])
        );
        assert!(h.reads.journal().unwrap().local_unread.is_empty());
        // Nothing left to clear: no writes.
        let revision = h.reads.revision();
        assert!(
            h.unread
                .mark_all_channels_read(&mut h.reads, 60, T)
                .unwrap()
                .is_empty()
        );
        assert_eq!(h.reads.revision(), revision);
    }

    #[test]
    fn bottom_catch_up_keeps_attention_and_later_activity_unread() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let viewer = h.vpk();
        let own = h.mine("room", "my thread", 11, vec![]);
        let reply = h.alice(
            "room",
            "reply to me",
            12,
            reply_tags(&own.id_hex(), &own.id_hex()),
        );
        let ordinary_root = h.alice("room", "their thread", 13, vec![]);
        let o = ordinary_root.id_hex();
        let ordinary_reply = h.alice("room", "ordinary reply", 14, reply_tags(&o, &o));
        let mention = h.alice("room", "mention", 15, tags(&[&["p", &viewer]]));
        let broadcast = h.alice("room", "broadcast", 16, tags(&[&["broadcast", "1"]]));
        let bottom = h.alice("room", "bottom", 20, vec![]);
        h.emit(&[
            own,
            reply.clone(),
            ordinary_root,
            ordinary_reply.clone(),
            mention,
            broadcast,
            bottom.clone(),
        ]);
        let mut lease = h.unread.reading("room", &h.reads).unwrap();
        h.unread
            .catch_up(&mut h.reads, &mut lease, &bottom.id_hex(), None, T)
            .unwrap();
        assert_eq!(
            h.frontiers(),
            BTreeMap::from([("activity:room".to_string(), 20)])
        );
        assert_eq!(h.counts(&room()), (Some(3), Some(3)));
        assert!(h.attention("room", &reply).unread);
        assert_eq!(h.counts(&ReadTarget::thread("room", &o)).0, Some(0));
        let late_old = h.alice("room", "late old ordinary", 19, vec![]);
        let new = h.alice("room", "new", 21, vec![]);
        h.emit(&[late_old, new]);
        assert_eq!(h.counts(&room()), (Some(4), Some(3)));
        // Participation found later restores attention, not catch-up.
        let earlier = h.mine("room", "earlier participation", 13, reply_tags(&o, &o));
        h.emit(&[earlier]);
        let attention = h.attention("room", &ordinary_reply);
        assert_eq!(
            (attention.category, attention.unread),
            (Some(Category::Thread), true)
        );
        h.unread.end_reading(lease);
    }

    #[test]
    fn bottom_catch_up_ends_a_manual_channel_unread() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let marked = h.alice("room", "marked", 11, vec![]);
        let bottom = h.alice("room", "bottom", 12, vec![]);
        h.emit(&[marked.clone(), bottom.clone()]);
        let target = ReadTarget::message("room", marked.id_hex());
        h.unread
            .mark_unread_local(&mut h.reads, &target, T)
            .unwrap();
        h.unread
            .mark_unread_local(&mut h.reads, &room(), T)
            .unwrap();
        let mut lease = h.unread.reading("room", &h.reads).unwrap();
        h.unread
            .catch_up(&mut h.reads, &mut lease, &bottom.id_hex(), None, T)
            .unwrap();
        assert_eq!(h.reads.local_unread("room"), None);
        assert!(h.reads.local_unread(&target.key().unwrap()).is_some());
        assert_eq!(h.room().observed_count, Some(1));
    }

    #[test]
    fn own_messages_edits_and_reactions_leave_a_read_channel_read() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let mine = h.mine("room", "mine", 10, vec![]);
        let reply = h.mine(
            "room",
            "my reply",
            11,
            reply_tags(&mine.id_hex(), &mine.id_hex()),
        );
        h.emit(&[mine.clone(), reply]);
        assert_eq!(h.counts(&room()), (Some(0), Some(0)));
        let peer = h.alice("room", "hello", 12, vec![]);
        h.emit(std::slice::from_ref(&peer));
        assert_eq!(h.room().observed_count, Some(1));
        h.unread
            .mark_channel_read(&mut h.reads, "room", 12, T)
            .unwrap();
        assert!(!h.room().unread());
        // Later edits, reactions and the viewer's own posts are not new
        // messages.
        let alice = h.alice.clone();
        let edit = h.by(
            &alice,
            kinds::EDIT,
            Some("room"),
            "hello!",
            20,
            tags(&[&["e", &peer.id_hex()]]),
        );
        let reaction = h.by(
            &alice,
            kinds::REACTION,
            Some("room"),
            "+",
            21,
            tags(&[&["e", &mine.id_hex()]]),
        );
        let later = h.mine("room", "later", 22, vec![]);
        h.emit(&[edit, reaction, later]);
        assert!(!h.room().unread());
        // Deleting an unread message takes its unread with it.
        let gone = h.alice("room", "oops", 23, vec![]);
        h.emit(std::slice::from_ref(&gone));
        assert_eq!(h.room().observed_count, Some(1));
        let deletion = h.by(&alice, 5, None, "", 24, tags(&[&["e", &gone.id_hex()]]));
        h.emit(&[deletion]);
        assert!(!h.room().unread());
    }

    #[test]
    fn a_newer_manual_unread_cancels_an_older_lease() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let bottom = h.alice("room", "bottom", 12, vec![]);
        h.emit(std::slice::from_ref(&bottom));
        let mut lease = h.unread.reading("room", &h.reads).unwrap();
        h.unread
            .mark_unread_local(&mut h.reads, &room(), T)
            .unwrap();
        h.unread
            .catch_up(&mut h.reads, &mut lease, &bottom.id_hex(), None, T)
            .unwrap();
        h.unread
            .observe(&mut h.reads, &mut lease, &[bottom.id_hex()], T)
            .unwrap();
        assert!(h.frontiers().is_empty());
        assert_eq!(h.room().manual, Manual::LocalOnly);
    }

    #[test]
    fn reading_a_dm_reads_all_of_it() {
        for catch_up in [true, false] {
            let mut h = H::new();
            h.grant(&[("room", Some("dm"))]);
            let first = h.alice("room", "first", 11, vec![]);
            let reply = h.alice(
                "room",
                "reply",
                14,
                reply_tags(&first.id_hex(), &first.id_hex()),
            );
            let row = h.alice("room", "direct", 12, vec![]);
            h.emit(&[first, reply, row.clone()]);
            assert_eq!(h.counts(&room()), (Some(3), Some(3)));
            h.unread
                .mark_unread_local(&mut h.reads, &room(), T)
                .unwrap();
            let mut lease = h.unread.reading("room", &h.reads).unwrap();
            if catch_up {
                h.unread
                    .catch_up(&mut h.reads, &mut lease, &row.id_hex(), None, T)
                    .unwrap();
            } else {
                h.unread
                    .observe(&mut h.reads, &mut lease, &[row.id_hex()], T)
                    .unwrap();
            }
            assert_eq!(h.frontiers(), BTreeMap::from([("room".to_string(), 14)]));
            let snapshot = h.room();
            assert_eq!(
                (
                    snapshot.observed_count,
                    snapshot.attention_count,
                    snapshot.manual
                ),
                (Some(0), Some(0), Manual::None)
            );
            assert!(snapshot.badge(true).priority);
            assert!(!snapshot.unread());
        }
    }

    #[test]
    fn catch_up_replaces_ordinary_message_marks_and_keeps_attention_marks() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let viewer = h.vpk();
        let first = h.alice("room", "first", 11, vec![]);
        let mention = h.alice("room", "mention", 12, tags(&[&["p", &viewer]]));
        let bottom = h.alice("room", "bottom", 13, vec![]);
        h.emit(&[first.clone(), mention.clone(), bottom.clone()]);
        let mut lease = h.unread.reading("room", &h.reads).unwrap();
        let ids = [first.id_hex(), mention.id_hex()];
        h.unread.observe(&mut h.reads, &mut lease, &ids, T).unwrap();
        h.unread
            .catch_up(&mut h.reads, &mut lease, &bottom.id_hex(), None, T)
            .unwrap();
        assert_eq!(
            h.frontiers(),
            BTreeMap::from([
                (format!("msg:{}", mention.id_hex()), 12),
                ("activity:room".to_string(), 13),
            ])
        );
        assert_eq!(h.room().observed_count, Some(0));
        assert!(!h.attention("room", &first).unread);
        h.unread.end_reading(lease);
        // Reading covered messages again saves nothing.
        let revision = h.reads.revision();
        let mut again = h.unread.reading("room", &h.reads).unwrap();
        h.unread.observe(&mut h.reads, &mut again, &ids, T).unwrap();
        assert_eq!(h.reads.revision(), revision);
        // A channel mark covers everything, its catch-up mark included.
        h.unread
            .mark_channel_read(&mut h.reads, "room", 30, T)
            .unwrap();
        assert_eq!(h.frontiers(), BTreeMap::from([("room".to_string(), 30)]));
    }

    #[test]
    fn a_peer_channel_mark_drops_the_local_marks_it_covers() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let first = h.alice("room", "first", 11, vec![]);
        let later = h.alice("room", "later", 40, vec![]);
        h.emit(&[first.clone(), later.clone()]);
        let mut lease = h.unread.reading("room", &h.reads).unwrap();
        let ids = [first.id_hex(), later.id_hex()];
        h.unread.observe(&mut h.reads, &mut lease, &ids, T).unwrap();
        let blob = json!({"v": 1, "client_id": "peer", "contexts": {"room": 30}});
        let marker = sign_read_state(&h.viewer, &"b".repeat(32), 30, &blob, 30).unwrap();
        let marker = h.verifier.verify(marker).unwrap();
        let coverage = h.unread.coverage();
        assert!(h.reads.accept(&[marker], &h.viewer, Some(&coverage)));
        assert_eq!(
            h.frontiers(),
            BTreeMap::from([
                ("room".to_string(), 30),
                (format!("msg:{}", later.id_hex()), 40)
            ])
        );
        assert!(!h.attention("room", &first).unread);
        assert!(!h.attention("room", &later).unread);
    }

    #[test]
    fn message_unread_forces_a_subtree_for_the_visit_and_read_clears_it() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let root = h.alice("room", "root", 10, vec![]);
        let r = root.id_hex();
        let reply = h.alice("room", "reply", 11, reply_tags(&r, &r));
        let mine = h.mine("room", "mine", 12, reply_tags(&r, &reply.id_hex()));
        h.emit(&[root.clone(), reply.clone(), mine.clone()]);
        h.unread
            .mark_channel_read(&mut h.reads, "room", 20, T)
            .unwrap();
        assert_eq!(h.room().observed_count, Some(0));
        h.unread.enter_channel(&mut h.reads, "room", T).unwrap();
        h.unread
            .mark_message_unread(&mut h.reads, "room", &r, T)
            .unwrap();
        // Root and the peer reply; never the viewer's own row.
        assert_eq!(h.room().observed_count, Some(2));
        let attention = h.attention("room", &mine);
        assert!(attention.forced && !attention.unread);
        assert_eq!(h.room().manual, Manual::LocalOnly);
        // Leaving drops the rows; the sidebar hint stays until reopening.
        h.unread.leave_channel("room");
        let snapshot = h.room();
        assert_eq!(
            (snapshot.observed_count, snapshot.manual),
            (Some(0), Manual::LocalOnly)
        );
        h.unread.enter_channel(&mut h.reads, "room", T).unwrap();
        assert_eq!(h.room().manual, Manual::None);
        // Read clears the subtree's marks atomically.
        h.unread
            .mark_message_unread(&mut h.reads, "room", &r, T)
            .unwrap();
        h.unread
            .mark_message_read(&mut h.reads, "room", &r, T)
            .unwrap();
        let snapshot = h.room();
        assert_eq!(
            (snapshot.observed_count, snapshot.manual),
            (Some(0), Manual::None)
        );
        let frontiers = h.frontiers();
        assert_eq!(frontiers.get(&format!("msg:{}", mine.id_hex())), None);
    }

    #[test]
    fn the_inbox_groups_conversations_and_keeps_resume_points() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream")), ("dm", Some("dm"))]);
        let viewer = h.vpk();
        let root = h.mine("room", "My thread", 20, vec![]);
        let r = root.id_hex();
        let mention = h.alice("room", "Mention", 21, tags(&[&["p", &viewer]]));
        let reply = h.alice("room", "First reply", 22, reply_tags(&r, &r));
        let mut newer_tags = reply_tags(&r, &r);
        newer_tags.push(vec!["p".into(), viewer]);
        let newer = h.alice("room", "Mentioned reply", 23, newer_tags);
        let dm = h.alice("dm", "Direct hello", 24, vec![]);
        let noise = h.alice("room", "Not relevant", 25, vec![]);
        h.emit(&[root, mention, reply.clone(), newer.clone(), dm, noise]);
        let before = h.unread.inbox(&h.reads);
        assert_eq!(before.items.len(), 3);
        assert_eq!(before.status, InboxStatus::Ready);
        let thread = before
            .items
            .iter()
            .find(|item| item.thread)
            .unwrap()
            .clone();
        assert_eq!(thread.message_id, reply.id_hex());
        assert_eq!(thread.latest_message_id, newer.id_hex());
        assert!(thread.mentioned);
        assert_eq!(thread.unread_count, 2);
        assert_eq!(thread.preview, "First reply");
        assert_eq!(thread.target, ReadTarget::thread("room", &r));
        // Each member's time and mention, for the archive (#677).
        let facts = [(reply.id_hex(), 22, false), (newer.id_hex(), 23, true)].map(
            |(id, created_at, mentioned)| InboxMessage {
                id,
                created_at,
                mentioned,
            },
        );
        assert_eq!(thread.messages, facts);
        assert_eq!(
            thread.message_ids,
            thread
                .messages
                .iter()
                .map(|m| m.id.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(before.items[0].channel_id, "dm");
        assert_eq!(before.items[0].target, ReadTarget::channel("dm"));
        assert!(Arc::ptr_eq(&before, &h.unread.inbox(&h.reads)));
        h.emit(std::slice::from_ref(&newer));
        assert!(Arc::ptr_eq(&before, &h.unread.inbox(&h.reads)));

        h.unread
            .mark_through(&mut h.reads, &thread.target, &thread.latest_message_id, T)
            .unwrap();
        let inbox = h.unread.inbox(&h.reads);
        let read = inbox
            .items
            .iter()
            .find(|item| item.id == thread.id)
            .unwrap();
        assert_eq!(
            (read.unread_count, read.message_id.as_str()),
            (0, newer.id_hex().as_str())
        );
        assert!(
            h.unread
                .activity("room", &h.reads)
                .items
                .as_ref()
                .unwrap()
                .is_empty()
        );
        h.unread
            .mark_unread_local(&mut h.reads, &thread.target, T)
            .unwrap();
        let inbox = h.unread.inbox(&h.reads);
        let marked = inbox
            .items
            .iter()
            .find(|item| item.id == thread.id)
            .unwrap();
        assert!(marked.manual && marked.unread_count == 0);
        h.unread
            .mark_through(&mut h.reads, &thread.target, &thread.latest_message_id, T)
            .unwrap();
        let inbox = h.unread.inbox(&h.reads);
        assert!(
            !inbox
                .items
                .iter()
                .find(|item| item.id == thread.id)
                .unwrap()
                .manual
        );
        assert_eq!(inbox.items.len(), 3);
        assert_eq!(h.room().observed_count, Some(2));

        // Archived channels leave the inbox but keep evidence and read state.
        let journal = h.reads.journal().unwrap().clone();
        let mut archived = ChannelInfo::joined("room", Some("stream"));
        archived.archived = true;
        h.unread
            .set_channels(vec![archived, ChannelInfo::joined("dm", Some("dm"))]);
        let inbox = h.unread.inbox(&h.reads);
        assert_eq!(
            inbox
                .items
                .iter()
                .map(|i| i.channel_id.as_str())
                .collect::<Vec<_>>(),
            ["dm"]
        );
        assert_eq!(h.room().observed_count, Some(2));
        assert_eq!(h.reads.journal().unwrap(), &journal);
    }

    #[test]
    fn following_a_thread_without_replying_counts_its_replies() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let root = h.alice("room", "root", 10, vec![]);
        let r = root.id_hex();
        let direct = h.alice("room", "direct", 20, reply_tags(&r, &r));
        let nested = h.alice("room", "nested", 30, reply_tags(&r, &direct.id_hex()));
        h.emit(&[root, direct.clone(), nested.clone()]);
        assert!(!h.unread.following("room", &r));
        h.room();
        h.attention("room", &nested);
        h.settle();
        let attention = h.attention("room", &nested);
        assert_eq!(
            (attention.status, attention.unread),
            (AttentionStatus::Ineligible, false)
        );
        assert_eq!(h.counts(&room()), (Some(1), Some(0)));

        h.unread.follow("room", &r, true).unwrap();
        assert!(h.unread.following("room", &r));
        for reply in [&direct, &nested] {
            let attention = h.attention("room", reply);
            assert_eq!(attention.status, AttentionStatus::Eligible);
            assert_eq!(attention.category, Some(Category::Thread));
            assert_eq!(attention.root_id.as_deref(), Some(r.as_str()));
            assert!(attention.unread);
        }
        assert_eq!(h.counts(&room()), (Some(3), Some(2)));
        let activity = h.unread.activity("room", &h.reads);
        assert_eq!(activity.items.as_ref().unwrap()[0].unread_count, 2);
        let inbox = h.unread.inbox(&h.reads);
        assert_eq!(inbox.items.len(), 1);
        assert!(inbox.items[0].thread);
    }

    #[test]
    fn unfollowing_overrides_participation_until_followed_again() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let viewer = h.vpk();
        let authored = h.mine("room", "authored", 10, vec![]);
        let a = authored.id_hex();
        let answer = h.alice("room", "answer", 20, reply_tags(&a, &a));
        let other = h.alice("room", "other", 11, vec![]);
        let o = other.id_hex();
        let mine = h.mine("room", "mine", 21, reply_tags(&o, &o));
        let sibling = h.alice("room", "sibling", 22, reply_tags(&o, &o));
        let nested_mine = h.mine("room", "nested mine", 23, reply_tags(&o, &sibling.id_hex()));
        h.emit(&[
            authored,
            answer.clone(),
            other,
            mine,
            sibling.clone(),
            nested_mine,
        ]);
        assert!(h.unread.following("room", &a));
        assert!(h.unread.following("room", &o));
        assert_eq!(
            h.attention("room", &answer).category,
            Some(Category::Thread)
        );

        h.unread.follow("room", &a, false).unwrap();
        h.unread.follow("room", &o, false).unwrap();
        assert!(!h.unread.following("room", &a));
        let attention = h.attention("room", &answer);
        assert_eq!(
            (attention.status, attention.unread),
            (AttentionStatus::Ineligible, false)
        );
        assert_eq!(h.attention("room", &sibling).category, None);
        assert!(
            h.unread
                .activity("room", &h.reads)
                .items
                .as_ref()
                .unwrap()
                .is_empty()
        );

        // Replying again does not undo an explicit unfollow; mentions still count.
        let later = h.mine("room", "later", 30, reply_tags(&a, &a));
        let peer_later = h.alice("room", "peer later", 31, reply_tags(&a, &later.id_hex()));
        let mut mention_tags = reply_tags(&a, &a);
        mention_tags.push(vec!["p".into(), viewer]);
        let mention = h.alice("room", "mention", 32, mention_tags);
        h.emit(&[later, peer_later.clone(), mention.clone()]);
        assert!(!h.unread.following("room", &a));
        assert_eq!(h.attention("room", &peer_later).category, None);
        let attention = h.attention("room", &mention);
        assert_eq!(
            (attention.category, attention.unread),
            (Some(Category::Mention), true)
        );

        h.unread.follow("room", &a, true).unwrap();
        let attention = h.attention("room", &peer_later);
        assert_eq!(
            (attention.category, attention.unread),
            (Some(Category::Thread), true)
        );
    }

    #[test]
    fn a_follow_that_cannot_be_saved_changes_nothing() {
        let viewer = Identity::ephemeral();
        let follows = MemoryThreadFollows {
            saved: Vec::new(),
            fail_writes: true,
        };
        let options = UnreadOptions {
            viewer: viewer.pubkey_hex(),
            ..UnreadOptions::default()
        };
        let mut unread = Unread::with_follows(options, follows);
        unread.set_channels(vec![ChannelInfo::joined("room", None)]);
        let root = "a".repeat(64);
        assert!(matches!(
            unread.follow("room", &root, true),
            Err(UnreadError::Follows(_))
        ));
        assert!(!unread.following("room", &root));
        assert_eq!(
            unread.follow("gone", &root, true),
            Err(UnreadError::ThreadUnavailable)
        );
        assert_eq!(
            unread.follow("room", "short", true),
            Err(UnreadError::ThreadUnavailable)
        );
    }

    #[test]
    fn thread_follow_records_parse_leniently_and_keep_the_newest() {
        let text = r#"[["c:a",true],["bad"],[1,true],["c:b",false],["c:a",false]]"#;
        assert_eq!(
            parse_thread_follows(text),
            vec![("c:a".to_string(), false), ("c:b".to_string(), false)]
        );
        assert!(parse_thread_follows("{").is_empty());
        let many: Vec<(String, bool)> = (0..1200).map(|n| (format!("c:{n}"), true)).collect();
        let parsed = parse_thread_follows(&encode_thread_follows(&many));
        assert_eq!(parsed.len(), THREAD_FOLLOW_LIMIT);
        assert_eq!(parsed[0].0, "c:200");
    }

    #[test]
    fn lookups_decide_replies_whose_parent_is_outside_the_window() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let parent = "c".repeat(64);
        let reply = h.alice(
            "room",
            "reply to an old message",
            20,
            reply_tags(&parent, &parent),
        );
        h.emit(std::slice::from_ref(&reply));
        let attention = h.attention("room", &reply);
        assert_eq!(attention.status, AttentionStatus::Unknown);
        assert!(attention.pending);
        assert!(h.unread.lookups_pending());
        let batch = h.unread.next_lookup_batch().unwrap();
        assert_eq!(batch.channel_id, "room");
        assert_eq!(batch.ids_filter.as_ref().unwrap()["ids"], json!([parent]));
        assert_eq!(batch.reply_targets.len(), 1);
        let filter = h.unread.replies_filter("room", &batch.reply_targets, None);
        assert_eq!(filter["authors"], json!([h.vpk()]));
        assert_eq!(filter["#e"], json!([parent]));
        // The viewer replied to that parent long ago.
        let old = h.mine("room", "my old reply", 5, reply_tags(&parent, &parent));
        h.unread
            .complete_lookup(batch, &[], std::slice::from_ref(&old));
        let attention = h.attention("room", &reply);
        assert_eq!(
            (attention.category, attention.pending),
            (Some(Category::Thread), false)
        );
        assert!(attention.unread);
        assert!(h.unread.following("room", &parent));
        // Deleting that reply ends the membership.
        let unsigned = h
            .viewer
            .template(
                Purpose::Content,
                5,
                30,
                tags(&[&["e", &old.id_hex()]]),
                String::new(),
            )
            .unwrap();
        let deletion = h
            .verifier
            .verify(h.viewer.sign(Purpose::Content, unsigned).unwrap())
            .unwrap();
        h.emit(&[deletion]);
        assert_eq!(h.attention("room", &reply).category, None);
    }

    #[test]
    fn failed_lookups_stay_pending_and_back_off() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let parent = "d".repeat(64);
        let reply = h.alice("room", "reply", 20, reply_tags(&parent, &parent));
        h.emit(std::slice::from_ref(&reply));
        h.attention("room", &reply);
        let batch = h.unread.next_lookup_batch().unwrap();
        assert_eq!(h.unread.fail_lookup(batch), Some(1000));
        assert!(h.attention("room", &reply).pending);
        let batch = h.unread.next_lookup_batch().unwrap();
        assert_eq!(h.unread.fail_lookup(batch), Some(2000));
        // A reset retires the batch without counting as a failure.
        let batch = h.unread.next_lookup_batch().unwrap();
        h.unread.stale();
        assert_eq!(h.unread.fail_lookup(batch), None);
    }

    #[test]
    fn reply_paging_splits_full_pages_and_pages_one_parent_back() {
        let h = H::new();
        let parent = "e".repeat(64);
        let rows: Vec<Verified> = (0..500)
            .map(|n| {
                h.mine(
                    "room",
                    "x",
                    1000 + n,
                    reply_tags(&"f".repeat(64), &"f".repeat(64)),
                )
            })
            .collect();
        let one = vec![ReplyTarget {
            parent_id: parent.clone(),
            root_id: None,
        }];
        let two = vec![one[0].clone(), one[0].clone()];
        assert_eq!(
            replies_paging(&one, &rows[..499], None, 0),
            RepliesPaging::Done
        );
        assert_eq!(
            replies_paging(&two, &rows, None, 0),
            RepliesPaging::Split(1)
        );
        assert_eq!(
            replies_paging(&one, &rows, None, 0),
            RepliesPaging::Older { until: 1000 }
        );
        assert_eq!(
            replies_paging(&one, &rows, Some(1000), 1),
            RepliesPaging::Done
        );
        assert_eq!(replies_paging(&one, &rows, None, 9), RepliesPaging::Done);
    }

    #[test]
    fn repair_samples_128_channels_per_read() {
        let mut h = H::new();
        let ids: Vec<String> = (0..278).map(|n| format!("c{n}")).collect();
        h.unread
            .set_channels(ids.iter().map(|id| ChannelInfo::joined(id, None)).collect());
        assert!(h.unread.needs_repair());
        let mut repair = h.unread.begin_repair().unwrap();
        assert!(h.unread.begin_repair().is_none());
        assert_eq!(h.unread.inbox(&h.reads).status, InboxStatus::Loading);
        let sizes: Vec<usize> = repair
            .filters
            .iter()
            .map(|filter| filter["#h"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, [128, 128, 22]);
        assert_eq!(repair.filters[0]["limit"], json!(500));
        assert_eq!(repair.filters[0]["include_aux"], json!(true));
        let row = h.alice("c0", "row", 10, vec![]);
        assert!(h.unread.repair_batch(&mut repair, &[row]));
        let end = h.unread.finish_repair(repair, Ok(()));
        assert_eq!(
            end,
            RepairEnd {
                again: false,
                find_homes: true
            }
        );
        assert_eq!(h.unread.freshness(), Freshness::Observed);
        assert_eq!(h.unread.inbox(&h.reads).status, InboxStatus::Ready);
        assert_eq!(
            h.snapshot(&ReadTarget::channel("c0")).observed_count,
            Some(1)
        );
        // A channel with no evidence is unknown, not zero.
        assert_eq!(h.snapshot(&ReadTarget::channel("c1")).observed_count, None);

        let repair = h.unread.begin_repair().unwrap();
        let end = h
            .unread
            .finish_repair(repair, Err("Relay is unreachable".into()));
        assert!(!end.find_homes);
        assert_eq!(h.unread.freshness(), Freshness::Stale);
        assert_eq!(h.unread.inbox(&h.reads).status, InboxStatus::Error);
    }

    #[test]
    fn confirming_a_cached_roster_asks_for_another_repair() {
        let mut h = H::new();
        let mut cached = ChannelInfo::joined("room", None);
        cached.cached = true;
        h.unread.set_channels(vec![cached]);
        let repair = h.unread.begin_repair().unwrap();
        assert!(repair.filters.is_empty());
        // Confirmed while the repair runs: it reruns afterwards.
        assert!(
            !h.unread
                .set_channels(vec![ChannelInfo::joined("room", None)])
        );
        let end = h.unread.finish_repair(repair, Ok(()));
        assert!(end.again);
    }

    #[test]
    fn overflow_clears_evidence_and_reports_it() {
        let mut h = H::new();
        h.grant(&[("room", None)]);
        let big = "x".repeat(1024 * 1024);
        let rows: Vec<Verified> = (0..9)
            .map(|n| h.alice("room", &big, 10 + n, vec![]))
            .collect();
        assert!(!h.unread.accept(&rows));
        assert_eq!(h.unread.freshness(), Freshness::Stale);
        let snapshot = h.room();
        assert_eq!(snapshot.observed_count, None);
        assert!(snapshot.error.as_deref().unwrap().contains("capacity"));
    }

    #[test]
    fn unchanged_snapshots_keep_their_identity() {
        let mut h = H::new();
        h.grant(&[("room", None), ("other", None)]);
        let row = h.alice("room", "row", 10, vec![]);
        h.emit(std::slice::from_ref(&row));
        let first = h.room();
        assert!(Arc::ptr_eq(&first, &h.room()));
        h.emit(&[row]);
        assert!(h.unread.index.is_some(), "a repeat keeps the index");
        let unrelated = h.alice("other", "elsewhere", 11, vec![]);
        h.emit(&[unrelated]);
        assert!(Arc::ptr_eq(&first, &h.room()));
        let newer = h.alice("room", "newer", 12, vec![]);
        h.emit(&[newer]);
        assert!(!Arc::ptr_eq(&first, &h.room()));
        assert_eq!(first.latest_message.as_ref().unwrap().created_at, 10);
        assert_eq!(h.room().latest_message.as_ref().unwrap().created_at, 12);
    }

    #[test]
    fn homes_are_found_for_marks_whose_messages_are_not_loaded() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let row = h.alice("room", "old", 10, vec![]);
        // A peer read this message and the whole channel later.
        let blob = json!({"v": 1, "client_id": "peer", "contexts": {
            "room": 20, format!("msg:{}", row.id_hex()): 10,
        }});
        let marker = sign_read_state(&h.viewer, &"a".repeat(32), 30, &blob, 30).unwrap();
        let marker = h.verifier.verify(marker).unwrap();
        let coverage = h.unread.coverage();
        assert!(h.reads.accept(&[marker], &h.viewer, Some(&coverage)));
        assert!(h.frontiers().contains_key(&format!("msg:{}", row.id_hex())));
        let request = h.unread.homes_request(&h.reads).unwrap();
        assert_eq!(request.filters[0]["ids"], json!([row.id_hex()]));
        let found = h
            .unread
            .homes_found(request, std::slice::from_ref(&row))
            .unwrap();
        let coverage = h.unread.coverage();
        h.reads.learn_homes(&found, Some(&coverage)).unwrap();
        assert_eq!(h.frontiers(), BTreeMap::from([("room".to_string(), 20)]));
        // Asked once per session.
        assert!(h.unread.homes_request(&h.reads).is_none());
    }

    #[test]
    fn the_journal_keeps_homes_only_for_named_messages() {
        let viewer = Identity::ephemeral();
        let mut journal = ReadJournal::new_random();
        journal.revision = 1;
        journal.homes.insert("a".repeat(64), "room".into());
        let loaded = ReadJournal::from_json(&journal.to_json(), &viewer.pubkey_hex()).unwrap();
        assert!(loaded.homes.is_empty());
    }

    #[test]
    fn workflow_owner_attribution_does_not_invent_mentions() {
        let relay = Keys::generate();
        let viewer = Identity::ephemeral();
        let vpk = viewer.pubkey_hex();
        let mut unread = Unread::new(UnreadOptions {
            viewer: vpk.clone(),
            relay_author: Some(relay.public_key().to_hex()),
            authority: Some(relay.public_key().to_hex()),
        });
        unread.set_channels(vec![ChannelInfo::joined("room", Some("stream"))]);
        let reads = ReadStateSync::open(
            vpk.clone(),
            ReadCapability::FrontierSync,
            MemoryJournal::default(),
        );
        let verifier = Verifier::default();
        let workflow = |mention: bool, at: u64| {
            let mut list = tags(&[
                &["h", "room"],
                &["p", &vpk],
                &["buzz:workflow", "true"],
                &["buzz:workflow-owner", &vpk],
            ]);
            if mention {
                list.push(vec!["buzz:workflow-mention".into(), vpk.clone()]);
            }
            let event = EventBuilder::new(Kind::from_u16(9), "output")
                .tags(list.into_iter().map(|tag| Tag::parse(tag).unwrap()))
                .custom_created_at(Timestamp::from_secs(at))
                .finalize(&relay)
                .unwrap();
            verifier.verify(event).unwrap()
        };
        let plain = workflow(false, 10);
        let mentioned = workflow(true, 11);
        assert!(unread.accept(&[plain.clone(), mentioned.clone()]));
        let attention = unread.attention("room", &plain.id_hex(), &reads);
        assert_eq!(attention.category, None);
        assert!(attention.unread);
        let attention = unread.attention("room", &mentioned.id_hex(), &reads);
        assert_eq!(attention.category, Some(Category::Mention));
        assert_eq!(
            workflow_owner(&plain, Some(&relay.public_key().to_hex())),
            Some(vpk)
        );
    }

    #[test]
    fn mute_never_changes_unread_and_spares_mentions() {
        // There is no mute input to unread; only notifications consult it.
        let direct = MessageAttention {
            category: Some(Category::Direct),
            ..MessageAttention::unknown()
        };
        assert!(direct.muted_suppresses());
        let mention = MessageAttention {
            category: Some(Category::Mention),
            mentioned: true,
            ..MessageAttention::unknown()
        };
        assert!(!mention.muted_suppresses());
    }

    /// Someone else's threads and top-level mentions, read in the channel
    /// timeline: nothing stays unread, and no thread dot remains.
    #[test]
    fn reading_the_timeline_leaves_no_dot_for_unfollowed_threads() {
        let mut h = H::new();
        h.grant(&[("room", Some("stream"))]);
        let viewer = h.vpk();
        let mut at = 10;
        let mut next = |h: &H, content: &str, extra: Vec<Vec<String>>| {
            at += 1;
            h.alice("room", content, at, extra)
        };
        let (mut top, mut replies) = (Vec::new(), Vec::new());
        for content in ["one", "two"] {
            top.push(next(&h, content, vec![]));
        }
        let root = next(&h, "root", vec![]);
        let r = root.id_hex();
        top.push(root);
        for content in ["a", "b", "c"] {
            replies.push(next(&h, content, reply_tags(&r, &r)));
        }
        top.push(next(&h, "@viewer", tags(&[&["p", &viewer]])));
        top.push(next(&h, "after", vec![]));
        for content in ["three", "four"] {
            top.push(next(&h, content, vec![]));
        }
        let second = next(&h, "second root", vec![]);
        let s = second.id_hex();
        top.push(second);
        for content in ["d", "e"] {
            replies.push(next(&h, content, reply_tags(&s, &s)));
        }
        replies.push(next(&h, "late", reply_tags(&r, &r)));
        top.push(next(&h, "@viewer again", tags(&[&["p", &viewer]])));
        top.push(next(&h, "final", vec![]));
        replies.extend(top.iter().cloned());
        h.emit(&replies);
        h.room();
        h.unread.activity("room", &h.reads);
        h.settle();
        assert_eq!(h.counts(&room()), (Some(top.len() as u32), Some(2)));
        assert_eq!(h.unread.activity("room", &h.reads).items, Some(vec![]));

        // The session's dwell: catch up the bottom, then observe each row.
        let mut lease = h.unread.reading("room", &h.reads).unwrap();
        let bottom = top.last().unwrap().id_hex();
        h.unread
            .catch_up(&mut h.reads, &mut lease, &bottom, None, T)
            .unwrap();
        for row in &top {
            let id = [row.id_hex()];
            h.unread.observe(&mut h.reads, &mut lease, &id, T).unwrap();
        }
        h.unread.end_reading(lease);
        assert_eq!(h.counts(&room()), (Some(0), Some(0)));
        assert_eq!(h.unread.activity("room", &h.reads).items, Some(vec![]));
    }

    #[test]
    fn badges_follow_the_sidebar_rules() {
        let mut snapshot = UnreadSnapshot::inaccessible(room());
        snapshot.observed_count = Some(2);
        snapshot.attention_count = Some(1);
        assert_eq!(
            snapshot.badge(false),
            Badge {
                unread: true,
                bold: true,
                priority: true
            }
        );
        snapshot.observed_count = Some(1);
        assert!(!snapshot.badge(false).bold);
        snapshot.observed_count = Some(0);
        snapshot.attention_count = Some(0);
        snapshot.manual = Manual::LocalOnly;
        assert_eq!(
            snapshot.badge(false),
            Badge {
                unread: true,
                bold: true,
                priority: false
            }
        );
    }
}
