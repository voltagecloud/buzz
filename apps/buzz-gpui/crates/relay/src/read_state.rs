//! NIP-RS read state: the wire event, the encrypted payload, the merge
//! algebra, retention, the local journal and the publish state machine.
//!
//! Ports buzz-app's `read-state-model.ts`, `read-state-retention.ts`,
//! `read-state-storage.ts`, `read-state.ts`, `read-state-snapshot.ts` and the
//! Tauri host codec (`src-tauri/src/identity.rs:573-880`). Behavior spec:
//! `docs/port/relay.md` §5i.1–5i.6.
//!
//! Everything here is synchronous. The session owns the network and the
//! clock: it calls [`ReadStateSync`] with `now` and with the results of the
//! reads and publishes that the state machine asks for
//! ([`PublishCommand`], [`RefreshRequest`]).
//!
//! # Overrides (§9.3 #9)
//!
//! Override registers (`ov_s`/`ov_c`/`ov_b`, manual unread from other
//! clients) are parsed and merged exactly as in the reference. This client
//! never creates them. Unlike the reference, a peer's override does not block
//! our publication: our slot publishes frontiers only and leaves registers to
//! the slots that own them. While any override is known, retention still keeps
//! every inherited channel and thread frontier in the published copy, so a
//! smaller publication can never reactivate a peer's register.

use crate::discovery::is_uuid;
use crate::event::{Verified, Verifier, is_hex64, kinds};
use crate::socket::{Filter, filter};
use buzz_identity::{Identity, Purpose};
use nostr::prelude::{Event, Keys};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// NIP-RS timestamps and counters are uint32.
pub const READ_STATE_MAX: u32 = u32::MAX;
/// Entries in one payload (`READ_STATE_KEYS`).
pub const READ_STATE_KEYS: usize = 10_000;
/// Largest signed read-state event.
pub const READ_STATE_EVENT_BYTES: usize = 64 * 1024;
/// Largest published plaintext (`READ_STATE_PLAINTEXT_BYTES`).
pub const READ_STATE_PLAINTEXT_BYTES: usize = 40 * 1024;
/// Largest payload accepted when parsing.
pub const READ_STATE_BLOB_BYTES: usize = 128 * 1024;
/// Budget of the journal's sync set (`retainRead` default).
pub const READ_JOURNAL_BYTES: usize = 96 * 1024;
/// Local-only receipts evicted from the sync set.
pub const READ_RESERVE_KEYS: usize = 5000;
pub const READ_RESERVE_BYTES: usize = 512 * 1024;
/// Largest raw event accepted for decoding.
pub const READ_DECODE_EVENT_BYTES: usize = 96 * 1024;
/// Debounce between a mutation and its publication (no max wait).
pub const PUBLISH_DEBOUNCE_MS: u64 = 5000;
/// Distinct coordinates remembered from the relay.
pub const READ_STATE_SLOTS: usize = 4096;
/// Complete snapshot bounds (NIP-11 `read_state_snapshot`).
pub const READ_SNAPSHOT_EVENTS: usize = 4096;
pub const READ_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
/// Cap on the snapshot response body, checked before JSON parsing.
pub const READ_SNAPSHOT_BODY_BYTES: usize = READ_SNAPSHOT_BYTES + 4096;
/// Most entries in `localUnread` and `recent`.
pub const LOCAL_UNREAD_KEYS: usize = 10_000;
/// The relay's exact refusal for a stale or future `created_at`.
pub const TIMESTAMP_REFUSAL: &str = "invalid: event timestamp too far from server time";
/// The signer refuses `created_at` further than this from now.
const SIGN_SKEW_SECS: u64 = 60;
/// Live replay limit for the read-state route (`LIVE_REPLAY_LIMIT`).
const READ_LIMIT: u64 = 500;
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
const SCOPED_SHARE: f64 = 0.75;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadStateError {
    #[error("Unsupported read-state blob")]
    UnsupportedBlob,
    #[error("{0}")]
    Capacity(&'static str),
    #[error("Invalid read frontier")]
    InvalidFrontier,
    #[error("Invalid read override")]
    InvalidOverride,
    #[error("Read override counter exhausted")]
    CounterExhausted,
    #[error("{0}")]
    InvalidTarget(&'static str),
    #[error("Read-state clock is ahead; retry when the clock catches up")]
    ClockAhead,
    #[error("Invalid read-state event")]
    InvalidEvent,
    #[error("Invalid read-state signing intent")]
    InvalidSigningIntent,
    #[error("Read-state signing failed: {0}")]
    Signing(String),
    #[error("{0}")]
    Signer(&'static str),
    #[error("{0}")]
    CorruptJournal(&'static str),
    #[error("Read-state storage: {0}")]
    Storage(String),
    #[error("Saved read state unavailable; retry storage first")]
    Unavailable,
    #[error("Read-state sync unsupported by this host")]
    SyncUnsupported,
    #[error("Reading observation expired")]
    Expired,
    #[error("Read-state slot conflict; publication blocked")]
    SlotConflict,
    #[error("Read-state accepted; coordinate observation still pending")]
    Unobserved,
    #[error("Read-state coordinate capacity exceeded")]
    TooManySlots,
    #[error("Complete read-state snapshot unavailable or mismatched")]
    SnapshotMismatch,
    #[error("{0}")]
    Snapshot(&'static str),
    /// A read or publish failure reported by the session.
    #[error("{0}")]
    Relay(String),
}

type Result<T, E = ReadStateError> = std::result::Result<T, E>;

// ---------------------------------------------------------------------------
// Model (`read-state-model.ts`)
// ---------------------------------------------------------------------------

/// A context key: non-empty, at most 256 UTF-8 bytes.
pub fn is_context_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256
}

/// A coordinate slot: 32 lowercase hex characters.
pub fn is_slot(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 1–128 ASCII letters, digits, `_` or `-`: a channel id a filter or key
/// may name.
pub(crate) fn is_channel_key(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What a read or an unread mark applies to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReadTarget {
    Channel {
        channel_id: String,
    },
    Thread {
        channel_id: String,
        root_id: String,
    },
    Message {
        channel_id: String,
        message_id: String,
    },
}

impl ReadTarget {
    pub fn channel(channel_id: impl Into<String>) -> Self {
        Self::Channel {
            channel_id: channel_id.into(),
        }
    }

    pub fn thread(channel_id: impl Into<String>, root_id: impl Into<String>) -> Self {
        Self::Thread {
            channel_id: channel_id.into(),
            root_id: root_id.into(),
        }
    }

    pub fn message(channel_id: impl Into<String>, message_id: impl Into<String>) -> Self {
        Self::Message {
            channel_id: channel_id.into(),
            message_id: message_id.into(),
        }
    }

    pub fn channel_id(&self) -> &str {
        match self {
            Self::Channel { channel_id }
            | Self::Thread { channel_id, .. }
            | Self::Message { channel_id, .. } => channel_id,
        }
    }

    /// The context key this target reads (`targetKey`): `<channel>`,
    /// `thread:<root>` or `msg:<id>`.
    pub fn key(&self) -> Result<String> {
        if !is_channel_key(self.channel_id()) {
            return Err(ReadStateError::InvalidTarget("Invalid read channel"));
        }
        let (prefix, id) = match self {
            Self::Channel { channel_id } => return Ok(channel_id.clone()),
            Self::Thread { root_id, .. } => ("thread", root_id),
            Self::Message { message_id, .. } => ("msg", message_id),
        };
        if !is_hex64(id) {
            return Err(ReadStateError::InvalidTarget("Invalid read event ID"));
        }
        Ok(format!("{prefix}:{id}"))
    }
}

/// A manual-unread register from another client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Override {
    pub set: u32,
    pub clear: u32,
    pub baseline: u32,
}

/// Parsed read state: frontiers ("read through this `created_at`") and
/// override registers, both keyed by context.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadState {
    pub frontiers: BTreeMap<String, u32>,
    pub overrides: BTreeMap<String, Override>,
}

impl ReadState {
    pub fn frontier(&self, key: &str) -> Option<u32> {
        self.frontiers.get(key).copied()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedBlob {
    pub client_id: String,
    pub state: ReadState,
}

fn escape_context(key: &str) -> String {
    if key.starts_with("ov_") || key.starts_with("esc:") {
        format!("esc:{key}")
    } else {
        key.to_owned()
    }
}

fn unescape_context(key: &str) -> &str {
    key.strip_prefix("esc:").unwrap_or(key)
}

/// A JSON number that is an integer in uint32 range (JS `uint32`).
fn uint32(value: &Value) -> Option<u32> {
    if let Some(n) = value.as_u64() {
        return u32::try_from(n).ok();
    }
    let f = value.as_f64()?;
    (f.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&f)).then_some(f as u32)
}

fn json_len<T: Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

/// The read-state coordinate (`read-state:<slot>`) of an event, if it is
/// one: kind 30078, exactly one `d` tag, exactly one `["t","read-state"]`
/// and a well-formed slot (`readCoordinate`). Other kind-30078 contracts
/// (sidebar preferences, kits) share the kind.
pub fn read_coordinate(event: &Event) -> Option<&str> {
    if event.kind.as_u16() != kinds::APP_DATA {
        return None;
    }
    let (mut d, mut ds, mut ts) = (None, 0, 0);
    for tag in event.tags.iter() {
        let tag = tag.as_slice();
        match tag.first().map(String::as_str) {
            Some("d") => {
                ds += 1;
                d = d.or(tag.get(1));
            }
            Some("t") if tag.get(1).map(String::as_str) == Some("read-state") => ts += 1,
            _ => {}
        }
    }
    let coordinate = d?.as_str();
    (ds == 1 && ts == 1 && coordinate.strip_prefix("read-state:").is_some_and(is_slot))
        .then_some(coordinate)
}

/// Parses a v1 payload (`parseReadBlob`). Unknown versions fail closed.
/// Override siblings are grouped before invalid entries are dropped, so a
/// malformed group is dropped whole; malformed frontiers are dropped alone.
pub fn parse_blob(raw: &Value) -> Result<ParsedBlob> {
    let object = raw.as_object().ok_or(ReadStateError::UnsupportedBlob)?;
    let client_id = object.get("client_id").and_then(Value::as_str);
    let contexts = object.get("contexts").and_then(Value::as_object);
    let (Some(client_id), Some(contexts)) = (client_id, contexts) else {
        return Err(ReadStateError::UnsupportedBlob);
    };
    let chars = client_id.chars().count();
    if object.get("v").and_then(uint32) != Some(1) || !(1..=64).contains(&chars) {
        return Err(ReadStateError::UnsupportedBlob);
    }
    if contexts.len() > READ_STATE_KEYS || json_len(raw) > READ_STATE_BLOB_BYTES {
        return Err(ReadStateError::Capacity("Read-state blob exceeds capacity"));
    }
    Ok(ParsedBlob {
        client_id: client_id.to_owned(),
        state: parse_contexts(contexts),
    })
}

fn parse_contexts(contexts: &Map<String, Value>) -> ReadState {
    let mut state = ReadState::default();
    let mut groups: BTreeMap<&str, BTreeMap<&str, Option<u32>>> = BTreeMap::new();
    for (key, value) in contexts {
        if let Some(prefix) = ["ov_s:", "ov_c:", "ov_b:"]
            .into_iter()
            .find(|prefix| key.starts_with(prefix))
        {
            let value = is_context_id(key).then(|| uint32(value)).flatten();
            groups
                .entry(&key[prefix.len()..])
                .or_default()
                .insert(prefix, value);
        } else if !key.starts_with("ov_")
            && is_context_id(key)
            && let Some(value) = uint32(value)
        {
            let entry = state
                .frontiers
                .entry(unescape_context(key).to_owned())
                .or_insert(0);
            *entry = (*entry).max(value);
        }
    }
    for (key, group) in groups {
        if !is_context_id(key) || group.values().any(Option::is_none) {
            continue;
        }
        let get = |prefix| group.get(prefix).copied().flatten().unwrap_or(0);
        if group.len() == 1 && group.contains_key("ov_c:") {
            state.overrides.insert(
                key.to_owned(),
                Override {
                    set: 0,
                    clear: get("ov_c:"),
                    baseline: 0,
                },
            );
        } else if group.len() == 3 {
            state.overrides.insert(
                key.to_owned(),
                Override {
                    set: get("ov_s:"),
                    clear: get("ov_c:"),
                    baseline: get("ov_b:"),
                },
            );
        }
    }
    state
}

/// Per-key max across states (`mergeReadStates`): commutative, idempotent
/// and monotone, so nothing can rewind a frontier.
pub fn merge_states(states: &[&ReadState]) -> Result<ReadState> {
    let mut merged = ReadState::default();
    for state in states {
        for (key, value) in &state.frontiers {
            let entry = merged.frontiers.entry(key.clone()).or_insert(0);
            *entry = (*entry).max(*value);
        }
        merge_overrides(&mut merged.overrides, &state.overrides);
    }
    if merged.frontiers.len() + 3 * merged.overrides.len() > READ_STATE_KEYS {
        return Err(ReadStateError::Capacity(
            "Read-state capacity reached; saved intent retained",
        ));
    }
    Ok(merged)
}

fn merge_overrides(into: &mut BTreeMap<String, Override>, from: &BTreeMap<String, Override>) {
    for (key, value) in from {
        let entry = into.entry(key.clone()).or_default();
        entry.set = entry.set.max(value.set);
        entry.clear = entry.clear.max(value.clear);
        entry.baseline = entry.baseline.max(value.baseline);
    }
}

/// The highest of the key's own frontier, its channel's and its thread's
/// (`effectiveFrontier`). `None` means unknown, which is not zero.
pub fn effective_frontier(
    state: &ReadState,
    key: &str,
    channel_id: Option<&str>,
    root_id: Option<&str>,
) -> Option<u32> {
    [
        state.frontier(key),
        channel_id.and_then(|channel| state.frontier(channel)),
        root_id.and_then(|root| state.frontier(&format!("thread:{root}"))),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// Whether a register forces unread (`overrideActive`).
pub fn override_active(value: Option<&Override>, frontier: Option<u32>) -> bool {
    value.is_some_and(|value| {
        value.set > 0 && value.set > value.clear && frontier.unwrap_or(0) <= value.baseline
    })
}

/// Merges one frontier into a state (`advanceRead`).
pub fn advance_read(state: &ReadState, key: &str, timestamp: u32) -> Result<ReadState> {
    if !is_context_id(key) {
        return Err(ReadStateError::InvalidFrontier);
    }
    let mut add = ReadState::default();
    add.frontiers.insert(key.to_owned(), timestamp);
    merge_states(&[state, &add])
}

/// Sets or clears an override register (`changeOverride`). The reference
/// has no caller outside tests and neither does this client; it is kept for
/// the algebra tests.
pub fn change_override(
    state: &ReadState,
    key: &str,
    unread: bool,
    frontier: u32,
) -> Result<ReadState> {
    if !is_context_id(&format!("ov_s:{key}")) {
        return Err(ReadStateError::InvalidOverride);
    }
    let old = state.overrides.get(key).copied().unwrap_or_default();
    let next = old
        .set
        .max(old.clear)
        .checked_add(1)
        .ok_or(ReadStateError::CounterExhausted)?;
    let mut add = ReadState::default();
    add.overrides.insert(
        key.to_owned(),
        if unread {
            Override {
                set: next,
                clear: 0,
                baseline: frontier,
            }
        } else {
            Override {
                set: 0,
                clear: next,
                baseline: 0,
            }
        },
    );
    merge_states(&[state, &add])
}

/// Encodes a payload for publication (`readBlob`). Frontier keys are
/// escaped; an active register emits all three entries, an inactive one only
/// its permanent clear floor. The result is re-parsed and must fit the 40 KiB
/// plaintext budget.
///
/// Keys are emitted in byte order (the reference sorts with
/// `localeCompare`); JSON object order carries no meaning.
pub fn encode_blob(
    client_id: &str,
    state: &ReadState,
    frontier: impl Fn(&str) -> Option<u32>,
) -> Result<Value> {
    let mut contexts = Map::new();
    for (key, value) in &state.frontiers {
        contexts.insert(escape_context(key), json!(value));
    }
    for (key, value) in &state.overrides {
        if !is_context_id(&format!("ov_s:{key}")) {
            return Err(ReadStateError::Capacity(
                "Read override key exceeds capacity",
            ));
        }
        if override_active(Some(value), frontier(key)) {
            contexts.insert(format!("ov_s:{key}"), json!(value.set));
            contexts.insert(format!("ov_c:{key}"), json!(value.clear));
            contexts.insert(format!("ov_b:{key}"), json!(value.baseline));
        } else if value.set != 0 || value.clear != 0 {
            contexts.insert(format!("ov_c:{key}"), json!(value.set.max(value.clear)));
        }
    }
    let blob = json!({ "v": 1, "client_id": client_id, "contexts": contexts });
    parse_blob(&blob)?;
    if json_len(&blob) > READ_STATE_PLAINTEXT_BYTES {
        return Err(ReadStateError::Capacity(
            "Read-state publication capacity reached; saved intent retained",
        ));
    }
    Ok(blob)
}

/// The next `created_at` for our slot (`readVersion`): strictly after the
/// previous one, but never more than 60 s ahead of the clock.
pub fn read_version(now: u64, previous: u64) -> Result<u32> {
    let next = now.max(previous + 1);
    if next > u64::from(READ_STATE_MAX) || next > now + SIGN_SKEW_SECS {
        return Err(ReadStateError::ClockAhead);
    }
    Ok(next as u32)
}

// ---------------------------------------------------------------------------
// Host codec (`src-tauri/src/identity.rs:573-880`)
// ---------------------------------------------------------------------------

/// Decrypts one of our own verified read-state events. Fails on anything
/// that is not exactly our NIP-RS coordinate.
pub fn decode_read_state(identity: &Identity, event: &Event) -> Result<ParsedBlob> {
    if event.kind.as_u16() != kinds::APP_DATA
        || event.pubkey != identity.public_key()
        || read_coordinate(event).is_none()
        || event.as_json().len() > READ_DECODE_EVENT_BYTES
    {
        return Err(ReadStateError::InvalidEvent);
    }
    let plaintext = identity
        .decrypt_from_self(event)
        .map_err(|_| ReadStateError::InvalidEvent)?;
    if plaintext.len() > READ_STATE_BLOB_BYTES {
        return Err(ReadStateError::Capacity(
            "Read-state plaintext capacity exceeded",
        ));
    }
    let blob: Value = serde_json::from_str(&plaintext).map_err(|_| ReadStateError::InvalidEvent)?;
    parse_blob(&blob)
}

/// Signs one slot's payload: kind 30078, NIP-44 v2 to self, tags exactly
/// `[["d","read-state:<slot>"],["t","read-state"]]`. Refuses a `created_at`
/// more than 60 s from `now` and plaintext over 40 KiB.
pub fn sign_read_state(
    identity: &Identity,
    slot: &str,
    created_at: u32,
    blob: &Value,
    now: u64,
) -> Result<Event> {
    if !is_slot(slot) || now.abs_diff(u64::from(created_at)) > SIGN_SKEW_SECS {
        return Err(ReadStateError::InvalidSigningIntent);
    }
    parse_blob(blob)?;
    let plaintext = blob.to_string();
    if plaintext.len() > READ_STATE_PLAINTEXT_BYTES {
        return Err(ReadStateError::Capacity(
            "Read-state publication capacity exceeded",
        ));
    }
    let signing = |error: buzz_identity::IdentityError| ReadStateError::Signing(error.to_string());
    let content = identity.encrypt_to_self(&plaintext).map_err(signing)?;
    let unsigned = identity
        .template(
            Purpose::AppData,
            kinds::APP_DATA,
            u64::from(created_at),
            vec![
                vec!["d".into(), format!("read-state:{slot}")],
                vec!["t".into(), "read-state".into()],
            ],
            content,
        )
        .map_err(signing)?;
    identity.sign(Purpose::AppData, unsigned).map_err(signing)
}

// ---------------------------------------------------------------------------
// Filters and the NIP-11 snapshot capability (`read-state-snapshot.ts`)
// ---------------------------------------------------------------------------

/// Bounded marker discovery: up to 500 of our read-state coordinates.
pub fn bounded_filter(viewer: &str) -> Filter {
    filter(json!({
        "kinds": [kinds::APP_DATA], "authors": [viewer], "#t": ["read-state"], "limit": READ_LIMIT,
    }))
}

/// The live route's read-state filter (`live.ts:600-608`).
pub fn live_filter(viewer: &str, since: u64) -> Filter {
    let mut filter = bounded_filter(viewer);
    filter.insert("since".into(), json!(since));
    filter
}

/// A fresh read of exactly our own slot.
pub fn own_slot_filter(viewer: &str, slot: &str) -> Filter {
    filter(json!({
        "kinds": [kinds::APP_DATA], "authors": [viewer],
        "#d": [format!("read-state:{slot}")], "limit": READ_LIMIT,
    }))
}

/// The complete-snapshot request body. The relay rejects any other shape.
pub fn snapshot_filter(viewer: &str) -> Vec<Filter> {
    vec![filter(json!({
        "kinds": [kinds::APP_DATA], "authors": [viewer], "read_state_snapshot": 1,
    }))]
}

/// The NIP-11 `read_state_snapshot` descriptor. Only version 1 with a
/// lowercase UUID community is accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotDescriptor {
    pub community_id: String,
}

impl SnapshotDescriptor {
    /// Reads the descriptor from a NIP-11 document.
    pub fn from_nip11(document: &Value) -> Option<Self> {
        let value = document.get("read_state_snapshot")?.as_object()?;
        let community = value.get("community_id")?.as_str()?;
        (value.get("version").and_then(uint32) == Some(1) && is_uuid(community)).then(|| Self {
            community_id: community.to_owned(),
        })
    }
}

/// Checks a complete snapshot response and verifies its events. Every event
/// must be ours, kind 30078, and have a distinct `d` coordinate.
pub fn parse_snapshot_response(
    raw: &Value,
    viewer: &str,
    community_id: &str,
    verifier: &Verifier,
) -> Result<Vec<Verified>> {
    let events = raw.get("events").and_then(Value::as_array);
    let snapshot_id = raw.get("snapshot_id").and_then(Value::as_str);
    let ok = raw.get("read_state_snapshot").and_then(uint32) == Some(1)
        && raw.get("complete") == Some(&Value::Bool(true))
        && raw.get("pubkey").and_then(Value::as_str) == Some(viewer)
        && raw.get("community_id").and_then(Value::as_str) == Some(community_id)
        && snapshot_id.is_some_and(is_hex64)
        && events.is_some_and(|events| {
            events.len() <= READ_SNAPSHOT_EVENTS && json_len(events) <= READ_SNAPSHOT_BYTES
        });
    let (true, Some(events)) = (ok, events) else {
        return Err(ReadStateError::SnapshotMismatch);
    };
    let mut coordinates = HashSet::new();
    let mut verified = Vec::with_capacity(events.len());
    for value in events {
        let event = verifier
            .verify_json(value)
            .map_err(|_| ReadStateError::InvalidEvent)?;
        if event.pubkey_hex() != viewer || event.kind_u16() != kinds::APP_DATA {
            return Err(ReadStateError::Snapshot(
                "Snapshot contains another principal or kind",
            ));
        }
        if !coordinates.insert(event.tag("d").unwrap_or_default().to_owned()) {
            return Err(ReadStateError::Snapshot(
                "Snapshot contains duplicate coordinates",
            ));
        }
        verified.push(event);
    }
    Ok(verified)
}

// ---------------------------------------------------------------------------
// Retention (`read-state-retention.ts`)
// ---------------------------------------------------------------------------

/// The mark that already reads everything `key` reads, if any. `frontier`
/// looks up marks being kept, plus `key` itself.
pub type Covered<'a> = dyn Fn(&str, &dyn Fn(&str) -> Option<u32>) -> Option<String> + 'a;

/// The message a mark names: `msg:`, `thread:` and `thread-activity:` each
/// name one message (a thread names its root).
pub fn mark_message(key: &str) -> Option<&str> {
    let (kind, id) = key.split_once(':')?;
    (!kind.is_empty() && matches!(kind, "msg" | "thread" | "thread-activity") && !id.is_empty())
        .then_some(id)
}

/// Broader marks first: channel, thread, catch-up, then message.
fn scope(key: &str) -> u8 {
    if !key.contains(':') {
        0
    } else if key.starts_with("thread:") {
        1
    } else if key.starts_with("activity:") || key.starts_with("thread-activity:") {
        2
    } else {
        3
    }
}

/// Cost of one `"key":value,` entry, separators included.
fn entry_cost(key: &str, value: u32) -> usize {
    json_len(key) + 2 + value.to_string().len()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retained {
    pub state: ReadState,
    pub recent: BTreeMap<String, u64>,
}

struct Budget {
    used: usize,
    keys: usize,
    max_bytes: usize,
}

impl Budget {
    fn take(&mut self, key: &str, value: u32, share: f64) -> bool {
        let size = entry_cost(key, value);
        if (self.used + size) as f64 > self.max_bytes as f64 * share
            || self.keys as f64 >= READ_STATE_KEYS as f64 * share
        {
            return false;
        }
        self.used += size;
        self.keys += 1;
        true
    }
}

/// Bounds a sync set to `max_bytes` (`retainRead`). Frontiers are bounded
/// recent hints, not a receipt log:
///
/// 1. Every override register and its direct frontier is kept, or this
///    fails; floors are never truncated.
/// 2. While any override exists, every non-`msg:` frontier is kept too
///    (inherited ancestry), and coverage pruning is off.
/// 3. Up to 75 % of the budget goes to broad marks first (channel, thread,
///    catch-up, message), stopping at the first that does not fit; the rest
///    goes by local interaction order (`recent`), then value, then key.
/// 4. Marks `covered` proves redundant are dropped once their cover is kept;
///    the cover inherits their recency and freed space is refilled.
///
/// Ties break by byte order where the reference uses `localeCompare`.
pub fn retain_read(
    states: &[&ReadState],
    recent: &BTreeMap<String, u64>,
    client_id: &str,
    max_bytes: usize,
    covered: Option<&Covered<'_>>,
) -> Result<Retained> {
    let mut frontiers: BTreeMap<String, u32> = BTreeMap::new();
    let mut overrides = BTreeMap::new();
    for state in states {
        for (key, value) in &state.frontiers {
            let entry = frontiers.entry(key.clone()).or_insert(0);
            *entry = (*entry).max(*value);
        }
        merge_overrides(&mut overrides, &state.overrides);
    }
    if 3 * overrides.len() > READ_STATE_KEYS {
        return Err(ReadStateError::Capacity(
            "Read-state capacity reached; saved intent retained",
        ));
    }
    let covered = if overrides.is_empty() { covered } else { None };
    let mut budget = Budget {
        used: json_len(&json!({ "v": 1, "client_id": client_id, "contexts": {} })),
        keys: 0,
        max_bytes,
    };
    let mut retained: BTreeMap<String, u32> = BTreeMap::new();
    let floors = "Read override capacity reached; saved floors retained";
    for (key, value) in &overrides {
        for (prefix, timestamp) in [
            ("ov_s:", value.set),
            ("ov_c:", value.clear),
            ("ov_b:", value.baseline),
        ] {
            if !budget.take(&format!("{prefix}{key}"), timestamp, 1.0) {
                return Err(ReadStateError::Capacity(floors));
            }
        }
        if let Some(&frontier) = frontiers.get(key) {
            if !budget.take(&escape_context(key), frontier, 1.0) {
                return Err(ReadStateError::Capacity(floors));
            }
            retained.insert(key.clone(), frontier);
        }
    }
    if !overrides.is_empty() {
        for (key, &value) in &frontiers {
            if retained.contains_key(key) || key.starts_with("msg:") {
                continue;
            }
            if !budget.take(&escape_context(key), value, 1.0) {
                return Err(ReadStateError::Capacity(
                    "Read override ancestry capacity reached; saved floors retained",
                ));
            }
            retained.insert(key.clone(), value);
        }
    }
    let mut next_recent = recent.clone();
    let by_use = |(a, av): &(&String, u32), (b, bv): &(&String, u32)| -> Ordering {
        let use_of = |key: &String| next_recent.get(key).copied().unwrap_or(0);
        use_of(b)
            .cmp(&use_of(a))
            .then(bv.cmp(av))
            .then_with(|| a.cmp(b))
    };
    let entries: Vec<(&String, u32)> = frontiers.iter().map(|(k, v)| (k, *v)).collect();
    let mut by_recent = entries.clone();
    by_recent.sort_by(by_use);
    let mut scoped = entries;
    scoped.sort_by(|a, b| scope(a.0).cmp(&scope(b.0)).then_with(|| by_use(a, b)));
    let mut dropped: HashSet<String> = HashSet::new();
    let select =
        |retained: &mut BTreeMap<String, u32>, budget: &mut Budget, dropped: &HashSet<String>| {
            for (key, value) in &scoped {
                if retained.contains_key(*key) || dropped.contains(*key) {
                    continue;
                }
                // Stop at the first broad mark that does not fit, so a narrower
                // mark never takes the share ahead of it.
                if !budget.take(&escape_context(key), *value, SCOPED_SHARE) {
                    break;
                }
                retained.insert((*key).clone(), *value);
            }
            for (key, value) in &by_recent {
                if !retained.contains_key(*key)
                    && !dropped.contains(*key)
                    && budget.take(&escape_context(key), *value, 1.0)
                {
                    retained.insert((*key).clone(), *value);
                }
            }
        };
    select(&mut retained, &mut budget, &dropped);
    if let Some(covered) = covered {
        loop {
            // Decide on one snapshot of the kept marks: a cover that did not
            // fit cannot replace anything.
            let mut covers: BTreeMap<String, String> = BTreeMap::new();
            for (key, &value) in &retained {
                let frontier = |other: &str| {
                    if other == key {
                        Some(value)
                    } else {
                        retained.get(other).copied()
                    }
                };
                if let Some(answer) = covered(key, &frontier)
                    && answer != *key
                    && retained.contains_key(&answer)
                {
                    covers.insert(key.clone(), answer);
                }
            }
            let resolve = |key: &String| -> String {
                let mut cover = covers.get(key).unwrap_or(key);
                let mut hops = 0;
                while let Some(next) = covers.get(cover) {
                    if hops >= covers.len() {
                        break;
                    }
                    cover = next;
                    hops += 1;
                }
                cover.clone()
            };
            let mut pruned = false;
            for key in covers.keys() {
                let cover = resolve(key);
                if cover == *key || covers.contains_key(&cover) {
                    continue;
                }
                let value = retained.remove(key).expect("covered marks are kept");
                dropped.insert(key.clone());
                pruned = true;
                budget.used -= entry_cost(&escape_context(key), value);
                budget.keys -= 1;
                if let Some(&used) = next_recent.get(key) {
                    let entry = next_recent.entry(cover).or_insert(0);
                    *entry = (*entry).max(used);
                }
            }
            if !pruned {
                break;
            }
            select(&mut retained, &mut budget, &dropped);
        }
    }
    next_recent.retain(|key, _| retained.contains_key(key));
    Ok(Retained {
        state: ReadState {
            frontiers: retained,
            overrides,
        },
        recent: next_recent,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalRetained {
    pub state: ReadState,
    pub recent: BTreeMap<String, u64>,
    pub reserve: BTreeMap<String, u32>,
}

/// [`retain_read`] for the journal (96 KiB), with what does not fit moved to
/// the local-only reserve (`retainLocalRead`). Keys returning from the
/// reserve take the higher value. The reserve keeps inherited floors first
/// while overrides exist, then broad receipts before messages, newest event
/// first; it skips receipts a kept broader receipt covers and stops at 5,000
/// keys or 512 KiB.
pub fn retain_local_read(
    states: &[&ReadState],
    recent: &BTreeMap<String, u64>,
    client_id: &str,
    reserve: &BTreeMap<String, u32>,
    covered: Option<&Covered<'_>>,
) -> Result<LocalRetained> {
    let mut frontiers = reserve.clone();
    let override_keys: BTreeSet<&String> = states
        .iter()
        .flat_map(|state| state.overrides.keys())
        .collect();
    let mut returning = ReadState::default();
    for state in states {
        for (key, &value) in &state.frontiers {
            let previous = frontiers.get(key).copied();
            if let Some(previous) = previous {
                returning.frontiers.insert(key.clone(), previous);
            }
            frontiers.insert(key.clone(), previous.unwrap_or(0).max(value));
        }
    }
    // Direct override floors belong in the journal.
    for key in &override_keys {
        if let Some(&value) = frontiers.get(*key) {
            returning.frontiers.insert((*key).clone(), value);
        }
    }
    let mut all: Vec<&ReadState> = states.to_vec();
    all.push(&returning);
    let kept = retain_read(&all, recent, client_id, READ_JOURNAL_BYTES, covered)?;
    for key in kept.state.frontiers.keys() {
        frontiers.remove(key);
    }
    let covered = if override_keys.is_empty() {
        covered
    } else {
        None
    };
    let protected = |key: &str| !override_keys.is_empty() && !key.starts_with("msg:");
    let mut candidates: Vec<(String, u32)> = frontiers.into_iter().collect();
    candidates.sort_by(|(a, av), (b, bv)| {
        protected(b)
            .cmp(&protected(a))
            .then(scope(a).cmp(&scope(b)))
            .then(bv.cmp(av))
            .then_with(|| a.cmp(b))
    });
    let mut accepted: BTreeMap<String, u32> = BTreeMap::new();
    let mut bytes = 2;
    for (key, value) in candidates {
        let redundant = covered.is_some_and(|covered| {
            let frontier = |other: &str| {
                if other == key {
                    Some(value)
                } else {
                    kept.state
                        .frontiers
                        .get(other)
                        .or_else(|| accepted.get(other))
                        .copied()
                }
            };
            covered(&key, &frontier).is_some_and(|cover| {
                cover != key
                    && (kept.state.frontiers.contains_key(&cover) || accepted.contains_key(&cover))
            })
        });
        if redundant {
            continue;
        }
        let cost = json_len(&key) + 1 + value.to_string().len() + usize::from(!accepted.is_empty());
        if accepted.len() >= READ_RESERVE_KEYS || bytes + cost > READ_RESERVE_BYTES {
            break;
        }
        accepted.insert(key, value);
        bytes += cost;
    }
    Ok(LocalRetained {
        state: kept.state,
        recent: kept.recent,
        reserve: accepted,
    })
}

// ---------------------------------------------------------------------------
// Journal (`read-state-storage.ts`)
// ---------------------------------------------------------------------------

/// Signed bytes saved before they are sent, with the revision they carry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPublish {
    pub event: Event,
    pub revision: u64,
}

/// Everything persisted per (community, viewer) partition. Stored as one
/// JSON record; the field names match the reference journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadJournal {
    pub version: u8,
    /// 32 hex; one per device and partition.
    pub slot: String,
    /// A UUID written into every payload this journal publishes.
    pub client_id: String,
    /// The sync set: what publication may send.
    pub state: ReadState,
    /// Local-only manual unread: key -> revision when marked. Keys are
    /// `<channel>`, `thread:<root>`, `msg:<id>` or `message-force:<channel>`.
    pub local_unread: BTreeMap<String, u64>,
    /// Revision of the last local interaction per sync-set key.
    #[serde(default)]
    pub recent: BTreeMap<String, u64>,
    /// Local-only frontiers evicted from the sync set.
    #[serde(default)]
    pub reserve: BTreeMap<String, u32>,
    /// Channel of each message a kept mark names (`mark_message`).
    #[serde(default)]
    pub homes: BTreeMap<String, String>,
    /// Local mutation counter.
    pub revision: u64,
    /// Highest revision reconciled with the relay.
    pub accepted_revision: u64,
    /// Last signed `created_at`.
    pub last_created_at: u32,
    /// Exact signed bytes awaiting publication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingPublish>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl ReadJournal {
    /// A new journal with a random slot and client ID.
    pub fn new_random() -> Self {
        let mut bytes = Keys::generate().secret_key().to_secret_bytes();
        bytes[22] = (bytes[22] & 0x0f) | 0x40;
        bytes[24] = (bytes[24] & 0x3f) | 0x80;
        let uuid = hex(&bytes[16..]);
        Self::new(
            hex(&bytes[..16]),
            format!(
                "{}-{}-{}-{}-{}",
                &uuid[..8],
                &uuid[8..12],
                &uuid[12..16],
                &uuid[16..20],
                &uuid[20..]
            ),
        )
    }

    pub fn new(slot: String, client_id: String) -> Self {
        Self {
            version: 1,
            slot,
            client_id,
            state: ReadState::default(),
            local_unread: BTreeMap::new(),
            recent: BTreeMap::new(),
            reserve: BTreeMap::new(),
            homes: BTreeMap::new(),
            revision: 0,
            accepted_revision: 0,
            last_created_at: 0,
            pending: None,
        }
    }

    pub fn coordinate(&self) -> String {
        format!("read-state:{}", self.slot)
    }

    /// Whether anything awaits publication.
    pub fn has_work(&self) -> bool {
        self.pending.is_some() || self.revision > self.accepted_revision
    }

    /// The state unread decisions use: reserve and sync set together (they
    /// are disjoint), with the sync set's overrides.
    pub fn local_state(&self) -> ReadState {
        let mut frontiers = self.reserve.clone();
        frontiers.extend(self.state.frontiers.iter().map(|(k, v)| (k.clone(), *v)));
        ReadState {
            frontiers,
            overrides: self.state.overrides.clone(),
        }
    }

    /// Parses and validates a saved record. Corrupt intent is an error,
    /// never an empty journal to overwrite (§9.2 #25).
    pub fn from_json(text: &str, viewer: &str) -> Result<Self> {
        let journal: Self = serde_json::from_str(text)
            .map_err(|_| ReadStateError::CorruptJournal("Invalid saved read state"))?;
        journal.checked(viewer)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("journals serialize")
    }

    /// Validates every invariant (`readJournal`) and drops `homes` entries
    /// that no kept mark names.
    pub fn checked(self, viewer: &str) -> Result<Self> {
        self.checked_over(viewer, None)
    }

    /// [`checked`](Self::checked) for a save over `previous`, which was
    /// checked already: an unchanged state is not round-tripped through
    /// the blob parser and an unchanged pending event is not verified
    /// again.
    fn checked_over(mut self, viewer: &str, previous: Option<&Self>) -> Result<Self> {
        let corrupt = ReadStateError::CorruptJournal;
        if self.version != 1
            || !is_slot(&self.slot)
            || self.revision > MAX_SAFE_INTEGER
            || self.accepted_revision > self.revision
        {
            return Err(corrupt("Invalid saved read state"));
        }
        let state_checked = previous.is_some_and(|previous| {
            previous.client_id == self.client_id && previous.state == self.state
        });
        if !state_checked {
            let mut contexts = Map::new();
            for (key, value) in &self.state.frontiers {
                contexts.insert(escape_context(key), json!(value));
            }
            for (key, value) in &self.state.overrides {
                contexts.insert(format!("ov_s:{key}"), json!(value.set));
                contexts.insert(format!("ov_c:{key}"), json!(value.clear));
                contexts.insert(format!("ov_b:{key}"), json!(value.baseline));
            }
            let blob = json!({ "v": 1, "client_id": self.client_id, "contexts": contexts });
            if parse_blob(&blob)?.state != self.state {
                return Err(corrupt("Invalid saved read entries"));
            }
        }
        let in_revision = |value: &u64| *value > 0 && *value <= self.revision;
        if self.local_unread.len() > LOCAL_UNREAD_KEYS
            || !self.local_unread.values().all(in_revision)
        {
            return Err(corrupt("Invalid saved read entries"));
        }
        if self.recent.len() > LOCAL_UNREAD_KEYS
            || !self
                .recent
                .iter()
                .all(|(key, value)| self.state.frontiers.contains_key(key) && in_revision(value))
        {
            return Err(corrupt("Invalid saved read interaction order"));
        }
        if self.reserve.len() > READ_RESERVE_KEYS
            || json_len(&self.reserve) > READ_RESERVE_BYTES
            || !self
                .reserve
                .keys()
                .all(|key| is_context_id(key) && !self.state.frontiers.contains_key(key))
        {
            return Err(corrupt("Invalid saved read reserve"));
        }
        if !self
            .homes
            .iter()
            .all(|(id, channel)| is_context_id(id) && is_context_id(channel))
        {
            return Err(corrupt("Invalid saved read mark channels"));
        }
        let named: HashSet<String> = self
            .state
            .frontiers
            .keys()
            .chain(self.reserve.keys())
            .filter_map(|key| mark_message(key).map(str::to_owned))
            .collect();
        self.homes.retain(|id, _| named.contains(id));
        if let Some(pending) = &self.pending {
            if pending.revision > self.revision {
                return Err(corrupt("Invalid saved read operation"));
            }
            let event = &pending.event;
            let verified = previous
                .and_then(|previous| previous.pending.as_ref())
                .is_some_and(|previous| previous.event.id == event.id);
            if event.pubkey.to_hex() != viewer
                || read_coordinate(event) != Some(self.coordinate().as_str())
                || event.created_at.as_secs() > u64::from(self.last_created_at)
                || (!verified && event.verify().is_err())
            {
                return Err(corrupt("Saved read operation identity mismatch"));
            }
        }
        Ok(self)
    }
}

/// Durable storage for one partition's journal: a single JSON record
/// ([`ReadJournal::to_json`]). The SQLite store keys it by the partition
/// `"<origin>:<viewer>"`. A store must fail rather than return a partial
/// record; validation happens in [`ReadStateSync`].
pub trait ReadStateJournal {
    fn load(&mut self) -> Result<Option<String>>;
    fn store(&mut self, record: &str) -> Result<()>;
    /// Deletes the partition, on community leave.
    fn purge(&mut self) -> Result<()>;
}

/// In-memory journal for tests.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    pub record: Option<String>,
    /// Fail this many upcoming stores.
    pub fail_stores: usize,
}

impl ReadStateJournal for MemoryJournal {
    fn load(&mut self) -> Result<Option<String>> {
        Ok(self.record.clone())
    }

    fn store(&mut self, record: &str) -> Result<()> {
        if self.fail_stores > 0 {
            self.fail_stores -= 1;
            return Err(ReadStateError::Storage("disk full".into()));
        }
        self.record = Some(record.to_owned());
        Ok(())
    }

    fn purge(&mut self) -> Result<()> {
        self.record = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Sync state machine (`read-state.ts`)
// ---------------------------------------------------------------------------

/// What this host can do with read state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadCapability {
    /// Cannot decode: no read state at all.
    Unsupported,
    /// Decodes but does not sign or publish.
    ReadOnly,
    FrontierSync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadSyncStatus {
    Loading,
    Local,
    Pending,
    /// Publish receipt only; not yet observed by readback.
    Accepted,
    /// Observed by readback.
    Reconciled,
    Stale,
    Error,
}

/// Completeness of marker enumeration (not of message history).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completeness {
    Unknown,
    /// A complete NIP-11 `read_state_snapshot`.
    Snapshot,
    /// An ordinary capped read.
    Bounded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadSyncSnapshot {
    pub capability: ReadCapability,
    pub status: ReadSyncStatus,
    pub completeness: Completeness,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncKind {
    Pending,
    LocalOnly,
}

/// A saved mutation. Durability is always "saved": failures are errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadMutationResult {
    /// `<slot>:<revision>`.
    pub operation_id: String,
    pub sync: SyncKind,
}

/// The channel (or thread) a broader mark covers marks under, as the
/// unread owner sees it. Registered by the session on each save.
pub trait MarkCoverage {
    /// Which broader mark covers `key`, as in [`Covered`]. `home` gives the
    /// channel of a message: from loaded evidence or a saved home.
    fn covered(
        &self,
        key: &str,
        frontier: &dyn Fn(&str) -> Option<u32>,
        home: &dyn Fn(&str) -> Option<String>,
    ) -> Option<String>;
    /// The channel of a loaded, verified message.
    fn home(&self, id: &str) -> Option<String>;
}

/// Read access the unread owner needs.
pub trait ReadLookup {
    /// The effective local state (sync set plus reserve).
    fn state(&self) -> &ReadState;
    fn local_unread(&self, key: &str) -> Option<u64>;
    fn revision(&self) -> u64;
    fn home(&self, id: &str) -> Option<&str>;
    fn capability(&self) -> ReadCapability;
    /// Bumps on every change to the journal or health.
    fn changes(&self) -> u64;
}

/// Local unread intent newer than `revision` on `key` cancels the update
/// (a reading lease that started before a manual "mark unread").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManualFence {
    pub key: String,
    pub revision: u64,
}

/// One message in an atomic subtree read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageRead {
    pub key: String,
    pub timestamp: u32,
    pub channel_id: String,
    pub root_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum UpdateKind {
    /// `mutate`: one key, an optional frontier.
    Single { key: String, timestamp: Option<u32> },
    /// `readMessages`: a subtree, atomically.
    Messages(Vec<MessageRead>),
}

/// A read-state mutation, built by the unread owner and applied with
/// [`ReadStateSync::apply`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadUpdate {
    kind: UpdateKind,
    clear_local: Vec<String>,
    mark_local: bool,
    fences: Vec<ManualFence>,
}

impl ReadUpdate {
    /// An automatic observation: advances a frontier, clears nothing.
    pub fn observe(key: impl Into<String>, timestamp: u32) -> Self {
        Self {
            kind: UpdateKind::Single {
                key: key.into(),
                timestamp: Some(timestamp),
            },
            clear_local: Vec::new(),
            mark_local: false,
            fences: Vec::new(),
        }
    }

    /// An explicit read: advances a frontier and clears local manual unread
    /// on `clear` (the key itself when empty).
    pub fn read(key: impl Into<String>, timestamp: u32, clear: Vec<String>) -> Self {
        let key = key.into();
        let clear = if clear.is_empty() {
            vec![key.clone()]
        } else {
            clear
        };
        Self {
            clear_local: clear,
            ..Self::observe(key, timestamp)
        }
    }

    /// Clears local manual unread without a frontier.
    pub fn clear_local(key: impl Into<String>, clear: Vec<String>) -> Self {
        Self {
            kind: UpdateKind::Single {
                key: key.into(),
                timestamp: None,
            },
            clear_local: clear,
            mark_local: false,
            fences: Vec::new(),
        }
    }

    /// Marks `key` unread on this device only.
    pub fn mark_local(key: impl Into<String>) -> Self {
        Self {
            mark_local: true,
            ..Self::clear_local(key, Vec::new())
        }
    }

    /// Reads a subtree of messages atomically and clears their local marks
    /// (plus `clear_force`). On a host without frontier sync, succeeds only
    /// if every message is already read, and then writes no frontier.
    pub fn messages(messages: Vec<MessageRead>, clear_force: Option<String>) -> Self {
        Self {
            kind: UpdateKind::Messages(messages),
            clear_local: clear_force.into_iter().collect(),
            mark_local: false,
            fences: Vec::new(),
        }
    }

    pub fn fenced(mut self, key: impl Into<String>, revision: u64) -> Self {
        self.fences.push(ManualFence {
            key: key.into(),
            revision,
        });
        self
    }

    fn writes_frontier(&self) -> bool {
        match &self.kind {
            UpdateKind::Single { timestamp, .. } => timestamp.is_some(),
            UpdateKind::Messages(messages) => !messages.is_empty(),
        }
    }
}

/// How a publish attempt failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishFailure {
    /// The relay refused `created_at` with [`TIMESTAMP_REFUSAL`].
    TimestampRejected,
    /// Anything else, including a lost response: retry the same bytes.
    Other(String),
}

impl PublishFailure {
    /// Classifies a non-2xx `POST /events` response (`transport.ts:1289`):
    /// a 400, or a 503 with `sent: false`, carrying exactly the timestamp
    /// refusal.
    pub fn from_response(status: u16, body: &Value) -> Self {
        let refusal = body.get("error").and_then(Value::as_str) == Some(TIMESTAMP_REFUSAL);
        let unsent = body.get("sent") == Some(&Value::Bool(false));
        if refusal && (status == 400 || (status == 503 && unsent)) {
            Self::TimestampRejected
        } else {
            Self::Other(
                body.get("error")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("HTTP {status}"), str::to_owned),
            )
        }
    }
}

/// Network work the state machine needs next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishCommand {
    /// `POST /events` with exactly these bytes; report with
    /// [`ReadStateSync::published`].
    Publish(Box<Event>),
    /// A fresh (uncached) read; report with [`ReadStateSync::read_back`].
    ReadBack(Vec<Filter>),
}

/// A marker discovery read for [`ReadStateSync::finish_refresh`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshRequest {
    epoch: u64,
    /// A complete-snapshot request (send `filters` as the exact `/query`
    /// body and parse with [`parse_snapshot_response`]); otherwise an
    /// ordinary bounded read.
    pub complete: bool,
    pub filters: Vec<Filter>,
}

#[derive(Clone, Debug)]
struct Slot {
    created_at: u64,
    id: nostr::prelude::EventId,
    client_id: String,
    state: ReadState,
}

impl Slot {
    fn newer_than(&self, other: &Slot) -> bool {
        self.created_at > other.created_at
            || (self.created_at == other.created_at && self.id < other.id)
    }
}

#[derive(Clone, Debug)]
enum Flush {
    Publishing {
        recover: bool,
        pending: PendingPublish,
    },
    ReadingBack {
        recover: bool,
        expired: bool,
        pending: PendingPublish,
    },
    ReadingOwn,
}

/// The single durable owner of one partition's read state.
///
/// - Mutations ([`Self::apply`]) save the whole journal before returning and
///   arm a 5 s debounce (no max wait; later mutations do not reset it).
/// - When the timer is due ([`Self::poll`]), or on an explicit
///   [`Self::flush`], publication runs as a sequence of
///   [`PublishCommand`]s: retry any saved pending bytes first, read back our
///   own slot, sign once, save the exact bytes, publish, read back again.
/// - Merging peers never bumps `revision`, so it never causes a publish.
pub struct ReadStateSync<J: ReadStateJournal> {
    viewer: String,
    capability: ReadCapability,
    store: J,
    journal: Option<ReadJournal>,
    local: ReadState,
    snapshot: ReadSyncSnapshot,
    slots: HashMap<String, Slot>,
    timer: Option<u64>,
    flush: Option<Flush>,
    attempted_revision: Option<u64>,
    failed_revision: Option<u64>,
    epoch: u64,
    requested: bool,
    refreshing: bool,
    changes: u64,
}

impl<J: ReadStateJournal> ReadStateSync<J> {
    /// Loads the journal (creating one if none is saved). A load failure
    /// leaves the owner in `error` with no journal; it never overwrites.
    pub fn open(viewer: impl Into<String>, capability: ReadCapability, mut store: J) -> Self {
        let viewer = viewer.into();
        let loaded = store.load().and_then(|record| match record {
            Some(text) => ReadJournal::from_json(&text, &viewer),
            None => {
                let journal = ReadJournal::new_random();
                store.store(&journal.to_json()).map(|()| journal)
            }
        });
        let mut owner = Self {
            viewer,
            capability,
            store,
            journal: None,
            local: ReadState::default(),
            snapshot: ReadSyncSnapshot {
                capability,
                status: ReadSyncStatus::Loading,
                completeness: Completeness::Unknown,
                error: None,
            },
            slots: HashMap::new(),
            timer: None,
            flush: None,
            attempted_revision: None,
            failed_revision: None,
            epoch: 0,
            requested: false,
            refreshing: false,
            changes: 0,
        };
        match loaded {
            Ok(journal) => {
                let status = if journal.has_work() {
                    ReadSyncStatus::Pending
                } else {
                    ReadSyncStatus::Local
                };
                owner.local = journal.local_state();
                owner.journal = Some(journal);
                owner.set_status(status, None);
            }
            Err(error) => {
                let message = match error {
                    ReadStateError::Storage(_) => error.to_string(),
                    other => format!("Read-state storage: {other}"),
                };
                owner.set_status(ReadSyncStatus::Error, Some(message));
            }
        }
        owner
    }

    pub fn viewer(&self) -> &str {
        &self.viewer
    }

    pub fn snapshot(&self) -> &ReadSyncSnapshot {
        &self.snapshot
    }

    pub fn journal(&self) -> Option<&ReadJournal> {
        self.journal.as_ref()
    }

    /// When the debounce timer fires, in the session's milliseconds.
    pub fn deadline(&self) -> Option<u64> {
        self.timer
    }

    /// Whether a publication is in progress.
    pub fn publishing(&self) -> bool {
        self.flush.is_some()
    }

    /// `ensure()`: true until the first refresh was requested.
    pub fn needs_refresh(&self) -> bool {
        !self.requested
    }

    /// Closes this owner, returning its storage.
    pub fn into_store(self) -> J {
        self.store
    }

    /// Deletes the partition (community leave) and closes this owner.
    pub fn purge(mut self) -> Result<()> {
        self.store.purge()
    }

    fn emit(&mut self) {
        self.changes += 1;
    }

    fn set_status(&mut self, status: ReadSyncStatus, error: Option<String>) {
        self.failed_revision = None;
        if self.snapshot.status != status || self.snapshot.error != error {
            self.snapshot.status = status;
            self.snapshot.error = error;
            self.emit();
        }
    }

    fn fail(&mut self, error: &ReadStateError) {
        self.set_status(ReadSyncStatus::Error, Some(error.to_string()));
    }

    fn save(&mut self, change: impl FnOnce(&ReadJournal) -> Result<ReadJournal>) -> Result<()> {
        let current = self.journal.as_ref().ok_or(ReadStateError::Unavailable)?;
        let next = change(current)?.checked_over(&self.viewer, Some(current))?;
        if Some(&next) == self.journal.as_ref() {
            return Ok(());
        }
        self.store.store(&next.to_json())?;
        self.local = next.local_state();
        self.journal = Some(next);
        self.emit();
        Ok(())
    }

    /// `retainLocalRead` plus the homes of the messages kept marks name.
    fn retain_marks(
        current: &ReadJournal,
        states: &[&ReadState],
        recent: &BTreeMap<String, u64>,
        learned: &BTreeMap<String, String>,
        coverage: Option<&dyn MarkCoverage>,
    ) -> Result<ReadJournal> {
        let home = |id: &str| -> Option<String> {
            learned
                .get(id)
                .or_else(|| current.homes.get(id))
                .cloned()
                .or_else(|| coverage.and_then(|coverage| coverage.home(id)))
        };
        let covered = |key: &str, frontier: &dyn Fn(&str) -> Option<u32>| {
            coverage.and_then(|coverage| coverage.covered(key, frontier, &home))
        };
        let kept = retain_local_read(
            states,
            recent,
            &current.client_id,
            &current.reserve,
            coverage.map(|_| &covered as &Covered<'_>),
        )?;
        let mut homes = BTreeMap::new();
        for key in kept.state.frontiers.keys().chain(kept.reserve.keys()) {
            if let Some(id) = mark_message(key)
                && let Some(channel) = home(id)
                && is_context_id(&channel)
            {
                homes.insert(id.to_owned(), channel);
            }
        }
        Ok(ReadJournal {
            state: kept.state,
            recent: kept.recent,
            reserve: kept.reserve,
            homes,
            ..current.clone()
        })
    }

    fn schedule(&mut self, now_ms: u64) {
        if self.capability == ReadCapability::FrontierSync
            && self.timer.is_none()
            && self.flush.is_none()
        {
            self.timer = Some(now_ms + PUBLISH_DEBOUNCE_MS);
        }
    }

    /// `reconcileJournal`: after publication or a peer save.
    fn reconcile(&mut self, now_ms: u64) {
        let Some(journal) = &self.journal else {
            return;
        };
        let (has_work, revision) = (journal.has_work(), journal.revision);
        let (accepted, last) = (journal.accepted_revision, journal.last_created_at);
        if has_work {
            if self
                .attempted_revision
                .is_none_or(|attempted| revision > attempted)
            {
                self.set_status(ReadSyncStatus::Pending, None);
                self.schedule(now_ms);
            }
        } else if matches!(
            self.snapshot.status,
            ReadSyncStatus::Pending | ReadSyncStatus::Accepted
        ) || self
            .failed_revision
            .is_some_and(|failed| accepted >= failed)
        {
            let status = if last > 0 {
                ReadSyncStatus::Reconciled
            } else {
                ReadSyncStatus::Local
            };
            self.set_status(status, None);
        }
    }

    /// Decodes our newest event per coordinate, merges them into the journal
    /// and remembers each slot. All or nothing.
    fn ingest(
        &mut self,
        events: &[Verified],
        identity: &Identity,
        coverage: Option<&dyn MarkCoverage>,
    ) -> Result<()> {
        let mut selected: BTreeMap<&str, &Event> = BTreeMap::new();
        for event in events {
            let event: &Event = event;
            if event.pubkey.to_hex() != self.viewer {
                continue;
            }
            if let Some(coordinate) = read_coordinate(event) {
                let current = selected.entry(coordinate).or_insert(event);
                if crate::event::is_newer(event, current) {
                    *current = event;
                }
            }
        }
        let decoded = selected
            .into_iter()
            .map(|(coordinate, event)| {
                decode_read_state(identity, event).map(|parsed| {
                    (
                        coordinate.to_owned(),
                        Slot {
                            created_at: event.created_at.as_secs(),
                            id: event.id,
                            client_id: parsed.client_id,
                            state: parsed.state,
                        },
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if decoded.is_empty() {
            return Ok(());
        }
        self.save(|current| {
            let mut states = vec![&current.state];
            states.extend(decoded.iter().map(|(_, slot)| &slot.state));
            Self::retain_marks(
                current,
                &states,
                &current.recent,
                &BTreeMap::new(),
                coverage,
            )
        })?;
        for (coordinate, slot) in decoded {
            match self.slots.get(&coordinate) {
                Some(old) if old.newer_than(&slot) => {}
                _ => {
                    self.slots.insert(coordinate, slot);
                }
            }
        }
        if self.slots.len() > READ_STATE_SLOTS {
            self.slots.clear();
            return Err(ReadStateError::TooManySlots);
        }
        Ok(())
    }

    /// Whether [`Self::accept`] would use anything in the batch: our own
    /// read-state coordinates, while sync is supported.
    pub fn relevant(&self, events: &[Verified]) -> bool {
        self.capability != ReadCapability::Unsupported
            && events
                .iter()
                .any(|event| event.pubkey_hex() == self.viewer && read_coordinate(event).is_some())
    }

    /// Feeds any verified batch (live or read). Only our own read-state
    /// coordinates are used; returns whether there were any.
    pub fn accept(
        &mut self,
        events: &[Verified],
        identity: &Identity,
        coverage: Option<&dyn MarkCoverage>,
    ) -> bool {
        if !self.relevant(events) {
            return false;
        }
        if let Err(error) = self.ingest(events, identity, coverage) {
            self.fail(&error);
        }
        true
    }

    /// Starts marker discovery: a complete snapshot when the relay
    /// advertises one, else a bounded read. `None` when unsupported or a
    /// refresh is already running.
    pub fn begin_refresh(
        &mut self,
        snapshot: Option<&SnapshotDescriptor>,
    ) -> Option<RefreshRequest> {
        self.requested = true;
        if self.capability == ReadCapability::Unsupported || self.refreshing {
            return None;
        }
        self.refreshing = true;
        let complete = snapshot.is_some();
        Some(RefreshRequest {
            epoch: self.epoch,
            complete,
            filters: if complete {
                snapshot_filter(&self.viewer)
            } else {
                vec![bounded_filter(&self.viewer)]
            },
        })
    }

    /// Completes a refresh with the verified events (or the read's error).
    pub fn finish_refresh(
        &mut self,
        request: RefreshRequest,
        result: std::result::Result<Vec<Verified>, String>,
        identity: &Identity,
        coverage: Option<&dyn MarkCoverage>,
        now_ms: u64,
    ) {
        if request.epoch != self.epoch {
            return;
        }
        self.refreshing = false;
        let outcome = result.map_err(ReadStateError::Relay).and_then(|events| {
            if self.journal.is_none() {
                return Err(ReadStateError::Unavailable);
            }
            self.ingest(&events, identity, coverage)
        });
        match outcome {
            Ok(()) => {
                let has_work = self.journal.as_ref().is_some_and(ReadJournal::has_work);
                self.snapshot.completeness = if request.complete {
                    Completeness::Snapshot
                } else {
                    Completeness::Bounded
                };
                let status = if has_work {
                    ReadSyncStatus::Pending
                } else {
                    ReadSyncStatus::Reconciled
                };
                self.set_status(status, None);
                self.emit();
                if has_work {
                    self.schedule(now_ms);
                }
            }
            Err(error) => self.fail(&error),
        }
    }

    /// Applies a mutation in one save. Frontier writes need
    /// [`ReadCapability::FrontierSync`]; local manual unread works anywhere.
    pub fn apply(
        &mut self,
        update: ReadUpdate,
        coverage: Option<&dyn MarkCoverage>,
        now_ms: u64,
    ) -> Result<ReadMutationResult> {
        if self.journal.is_none() {
            return Err(ReadStateError::Unavailable);
        }
        let sync = self.capability == ReadCapability::FrontierSync;
        let writes = update.writes_frontier();
        let result = self.save(|current| {
            if update.fences.iter().any(|fence| {
                current.local_unread.get(&fence.key).copied().unwrap_or(0) > fence.revision
            }) {
                return Err(ReadStateError::Expired);
            }
            let revision = current.revision + 1;
            let mut local_unread = current.local_unread.clone();
            let mut recent = current.recent.clone();
            let mut written = ReadState::default();
            match &update.kind {
                UpdateKind::Single { key, timestamp } => {
                    if timestamp.is_some() && !sync {
                        return Err(ReadStateError::SyncUnsupported);
                    }
                    if !is_context_id(key) {
                        return Err(ReadStateError::InvalidFrontier);
                    }
                    if let Some(timestamp) = timestamp {
                        recent.insert(key.clone(), revision);
                        written.frontiers.insert(key.clone(), *timestamp);
                    }
                    if update.mark_local {
                        local_unread.insert(key.clone(), revision);
                    }
                }
                UpdateKind::Messages(messages) => {
                    if messages.iter().any(|message| !is_context_id(&message.key)) {
                        return Err(ReadStateError::InvalidFrontier);
                    }
                    let state = current.local_state();
                    if !sync
                        && messages.iter().any(|message| {
                            effective_frontier(
                                &state,
                                &message.key,
                                Some(&message.channel_id),
                                message.root_id.as_deref(),
                            )
                            .is_none_or(|frontier| frontier < message.timestamp)
                        })
                    {
                        return Err(ReadStateError::SyncUnsupported);
                    }
                    for message in messages {
                        if sync {
                            recent.insert(message.key.clone(), revision);
                            let entry = written.frontiers.entry(message.key.clone()).or_insert(0);
                            *entry = (*entry).max(message.timestamp);
                        }
                        local_unread.remove(&message.key);
                    }
                }
            }
            if !update.mark_local {
                for key in &update.clear_local {
                    local_unread.remove(key);
                }
            }
            let mut next = if written.frontiers.is_empty() {
                ReadJournal {
                    recent,
                    ..current.clone()
                }
            } else {
                Self::retain_marks(
                    current,
                    &[&current.state, &written],
                    &recent,
                    &BTreeMap::new(),
                    coverage,
                )?
            };
            next.revision = revision;
            next.local_unread = local_unread;
            if written.frontiers.is_empty() && current.accepted_revision == current.revision {
                // Local-only changes never trigger a publish.
                next.accepted_revision = revision;
            }
            Ok(next)
        });
        if let Err(error) = result {
            // A rejected update changed nothing: only a failed save puts
            // sync in error (which holds notifications back).
            if !matches!(
                error,
                ReadStateError::Expired
                    | ReadStateError::SyncUnsupported
                    | ReadStateError::InvalidFrontier
            ) {
                self.fail(&error);
            }
            return Err(error);
        }
        let journal = self.journal.as_ref().expect("saved");
        let operation_id = format!("{}:{}", journal.slot, journal.revision);
        let status = if !sync {
            ReadSyncStatus::Local
        } else if journal.has_work() {
            ReadSyncStatus::Pending
        } else if journal.last_created_at > 0 {
            ReadSyncStatus::Reconciled
        } else {
            ReadSyncStatus::Local
        };
        self.set_status(status, None);
        if writes && sync {
            self.schedule(now_ms);
        }
        Ok(ReadMutationResult {
            operation_id,
            sync: if writes && sync {
                SyncKind::Pending
            } else {
                SyncKind::LocalOnly
            },
        })
    }

    /// Saves channels found for marked messages (from signed events) and
    /// drops marks they prove covered. Changes no read; publishes nothing.
    pub fn learn_homes(
        &mut self,
        found: &BTreeMap<String, String>,
        coverage: Option<&dyn MarkCoverage>,
    ) -> Result<()> {
        self.save(|current| {
            Self::retain_marks(current, &[&current.state], &current.recent, found, coverage)
        })
    }

    /// Ends the current marker observation (reconnect or access change).
    /// Late refresh results are ignored.
    pub fn stale(&mut self) {
        self.epoch += 1;
        self.refreshing = false;
        self.snapshot.completeness = Completeness::Unknown;
        self.set_status(ReadSyncStatus::Stale, None);
        self.emit();
    }

    /// Fires the debounce timer when due.
    pub fn poll(&mut self, now_ms: u64) -> Option<PublishCommand> {
        match self.timer {
            Some(deadline) if deadline <= now_ms => self.flush(),
            _ => None,
        }
    }

    /// Starts a publication now (also `retrySync`). `None` when there is
    /// nothing to do or one is already running.
    pub fn flush(&mut self) -> Option<PublishCommand> {
        if self.capability != ReadCapability::FrontierSync || self.flush.is_some() {
            return None;
        }
        self.timer = None;
        let journal = self.journal.as_ref()?;
        self.attempted_revision = Some(journal.revision);
        if let Some(pending) = journal.pending.clone() {
            // Unknown outcomes retry the exact bytes first.
            return Some(self.start_publish(pending, true));
        }
        if journal.revision <= journal.accepted_revision {
            return None;
        }
        self.flush = Some(Flush::ReadingOwn);
        Some(self.own_read_back())
    }

    fn own_read_back(&self) -> PublishCommand {
        let slot = self
            .journal
            .as_ref()
            .map_or("", |journal| journal.slot.as_str());
        PublishCommand::ReadBack(vec![own_slot_filter(&self.viewer, slot)])
    }

    fn start_publish(&mut self, pending: PendingPublish, recover: bool) -> PublishCommand {
        self.set_status(ReadSyncStatus::Pending, None);
        let event = Box::new(pending.event.clone());
        self.flush = Some(Flush::Publishing { recover, pending });
        PublishCommand::Publish(event)
    }

    /// Ends the publication with an error (also for timeouts, 10 s).
    pub fn abort_flush(&mut self, reason: &str, now_ms: u64) {
        if self.flush.is_some() {
            self.fail_flush(&ReadStateError::Relay(reason.to_owned()), now_ms);
        }
    }

    fn fail_flush(&mut self, error: &ReadStateError, now_ms: u64) {
        self.flush = None;
        self.fail(error);
        self.failed_revision = self
            .journal
            .as_ref()
            .filter(|journal| journal.has_work())
            .map(|journal| journal.revision);
        self.reconcile(now_ms);
    }

    fn finish_flush(&mut self, now_ms: u64) {
        self.flush = None;
        // Peer intent may have arrived during signing or readback.
        self.reconcile(now_ms);
    }

    /// Reports the outcome of [`PublishCommand::Publish`].
    pub fn published(
        &mut self,
        outcome: std::result::Result<(), PublishFailure>,
        now_ms: u64,
    ) -> Option<PublishCommand> {
        if !matches!(self.flush, Some(Flush::Publishing { .. })) {
            return None;
        }
        let Some(Flush::Publishing { recover, pending }) = self.flush.take() else {
            return None;
        };
        let now = now_ms / 1000;
        let expired = match outcome {
            Ok(()) => false,
            // A timestamp refusal also covers future skew. Only renew old
            // state, outside the signer's 60 s allowance, during recovery.
            Err(PublishFailure::TimestampRejected)
                if recover && pending.event.created_at.as_secs() < now.saturating_sub(60) =>
            {
                true
            }
            Err(PublishFailure::TimestampRejected) => {
                self.fail_flush(&ReadStateError::Relay(TIMESTAMP_REFUSAL.into()), now_ms);
                return None;
            }
            Err(PublishFailure::Other(error)) => {
                self.fail_flush(&ReadStateError::Relay(error), now_ms);
                return None;
            }
        };
        if !expired {
            self.set_status(ReadSyncStatus::Accepted, None);
        }
        self.flush = Some(Flush::ReadingBack {
            recover,
            expired,
            pending,
        });
        Some(self.own_read_back())
    }

    /// Reports the outcome of [`PublishCommand::ReadBack`].
    pub fn read_back(
        &mut self,
        outcome: std::result::Result<Vec<Verified>, String>,
        identity: &Identity,
        coverage: Option<&dyn MarkCoverage>,
        now_ms: u64,
    ) -> Option<PublishCommand> {
        if !matches!(
            self.flush,
            Some(Flush::ReadingOwn | Flush::ReadingBack { .. })
        ) {
            return None;
        }
        let flush = self.flush.take()?;
        let step = match outcome {
            Err(error) => Err(ReadStateError::Relay(error)),
            Ok(events) => match self.ingest(&events, identity, coverage) {
                Err(error) => Err(error),
                Ok(()) => match flush {
                    Flush::ReadingBack {
                        recover,
                        expired,
                        pending,
                    } => self.observe_pending(recover, expired, pending, identity, coverage),
                    _ => self.sign_pending(identity, coverage, now_ms).map(Some),
                },
            },
        };
        match step {
            Ok(Some(command)) => Some(command),
            Ok(None) => {
                self.finish_flush(now_ms);
                None
            }
            Err(error) => {
                self.fail_flush(&error, now_ms);
                None
            }
        }
    }

    fn own_slot(&self) -> Option<&Slot> {
        let journal = self.journal.as_ref()?;
        self.slots.get(&journal.coordinate())
    }

    /// After a publish: did our slot take the payload?
    fn observe_pending(
        &mut self,
        recover: bool,
        expired: bool,
        pending: PendingPublish,
        identity: &Identity,
        coverage: Option<&dyn MarkCoverage>,
    ) -> Result<Option<PublishCommand>> {
        let journal = self.journal.as_ref().ok_or(ReadStateError::Unavailable)?;
        let client_id = journal.client_id.clone();
        let payload = decode_read_state(identity, &pending.event)?;
        let own = self.own_slot();
        if own.is_some_and(|own| own.client_id != client_id) {
            return Err(ReadStateError::SlotConflict);
        }
        let observed = match own {
            Some(own) => merge_states(&[&own.state, &payload.state])? == own.state,
            None => false,
        };
        let id = pending.event.id;
        if !observed {
            if !expired {
                return Err(ReadStateError::Unobserved);
            }
            // Keep the old envelope until a replacement is saved; merge its
            // plaintext without acknowledging it.
            self.save(|current| {
                if current.pending.as_ref().map(|p| p.event.id) != Some(id) {
                    return Ok(current.clone());
                }
                Self::retain_marks(
                    current,
                    &[&current.state, &payload.state],
                    &current.recent,
                    &BTreeMap::new(),
                    coverage,
                )
            })?;
        } else {
            self.save(|current| {
                let mut next = current.clone();
                if current.pending.as_ref().map(|p| p.event.id) == Some(id) {
                    next.pending = None;
                    next.accepted_revision = current.accepted_revision.max(pending.revision);
                }
                Ok(next)
            })?;
            let journal = self.journal.as_ref().expect("saved");
            let status = if journal.revision > journal.accepted_revision {
                ReadSyncStatus::Pending
            } else {
                ReadSyncStatus::Reconciled
            };
            self.set_status(status, None);
        }
        if !recover {
            return Ok(None);
        }
        // Recovery finished; continue with newer intent, if any. An expired
        // envelope stays saved until its replacement is.
        let journal = self.journal.as_ref().expect("saved");
        if journal.revision <= journal.accepted_revision {
            return Ok(None);
        }
        self.flush = Some(Flush::ReadingOwn);
        Ok(Some(self.own_read_back()))
    }

    /// Builds, signs and saves the next payload, then asks to publish it.
    fn sign_pending(
        &mut self,
        identity: &Identity,
        coverage: Option<&dyn MarkCoverage>,
        now_ms: u64,
    ) -> Result<PublishCommand> {
        let journal = self.journal.as_ref().ok_or(ReadStateError::Unavailable)?;
        let own = self.own_slot();
        if own.is_some_and(|own| own.client_id != journal.client_id) {
            return Err(ReadStateError::SlotConflict);
        }
        let own_created_at = own.map_or(0, |own| own.created_at);
        let home = |id: &str| -> Option<String> {
            journal
                .homes
                .get(id)
                .cloned()
                .or_else(|| coverage.and_then(|coverage| coverage.home(id)))
        };
        let covered = |key: &str, frontier: &dyn Fn(&str) -> Option<u32>| {
            coverage.and_then(|coverage| coverage.covered(key, frontier, &home))
        };
        // Overrides stay in the retention input so their ancestry is kept,
        // but are not republished (§9.3 #9, see the module docs).
        let wire = retain_read(
            &[&journal.state],
            &journal.recent,
            &journal.client_id,
            READ_STATE_PLAINTEXT_BYTES,
            coverage.map(|_| &covered as &Covered<'_>),
        )?
        .state;
        let wire = ReadState {
            frontiers: wire.frontiers,
            overrides: BTreeMap::new(),
        };
        let blob = encode_blob(&journal.client_id, &wire, |key| wire.frontier(key))?;
        let now = now_ms / 1000;
        let previous = u64::from(journal.last_created_at).max(own_created_at);
        let created_at = read_version(now, previous)?;
        let (revision, slot) = (journal.revision, journal.slot.clone());
        self.attempted_revision = Some(revision);
        let event = sign_read_state(identity, &slot, created_at, &blob, now)?;
        // The signed output must be exactly the intent: same author,
        // coordinate and version, and a payload that decodes to this state.
        if event.pubkey != identity.public_key()
            || read_coordinate(&event) != Some(format!("read-state:{slot}").as_str())
            || event.created_at.as_secs() != u64::from(created_at)
        {
            return Err(ReadStateError::Signer(
                "Read-state signer changed identity or version",
            ));
        }
        let decoded = decode_read_state(identity, &event).ok();
        let intended = parse_blob(&blob)?;
        if decoded.is_none_or(|decoded| {
            decoded.client_id != journal.client_id || decoded.state != intended.state
        }) {
            return Err(ReadStateError::Signer(
                "Read-state signer changed the payload",
            ));
        }
        let pending = PendingPublish { event, revision };
        let saved = pending.clone();
        // The exact bytes are durable before they are sent.
        self.save(|current| {
            Ok(ReadJournal {
                last_created_at: created_at,
                pending: Some(saved),
                ..current.clone()
            })
        })?;
        Ok(self.start_publish(pending, false))
    }
}

impl<J: ReadStateJournal> ReadLookup for ReadStateSync<J> {
    fn state(&self) -> &ReadState {
        &self.local
    }

    fn local_unread(&self, key: &str) -> Option<u64> {
        self.journal.as_ref()?.local_unread.get(key).copied()
    }

    fn revision(&self) -> u64 {
        self.journal.as_ref().map_or(0, |journal| journal.revision)
    }

    fn home(&self, id: &str) -> Option<&str> {
        self.journal.as_ref()?.homes.get(id).map(String::as_str)
    }

    fn capability(&self) -> ReadCapability {
        self.capability
    }

    fn changes(&self) -> u64 {
        self.changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Kind, Tag, Timestamp};
    use std::collections::VecDeque;

    fn parse(contexts: Value) -> ReadState {
        parse_blob(&json!({ "v": 1, "client_id": "test", "contexts": contexts }))
            .unwrap()
            .state
    }

    fn state(frontiers: &[(&str, u32)]) -> ReadState {
        ReadState {
            frontiers: frontiers
                .iter()
                .map(|(key, value)| (key.to_string(), *value))
                .collect(),
            overrides: BTreeMap::new(),
        }
    }

    fn hex64(n: usize) -> String {
        format!("{n:064x}")
    }

    // -- model (read-state-model.test.ts) -----------------------------------

    #[test]
    fn merge_is_commutative_associative_and_idempotent() {
        let a =
            parse(json!({"channel": 11, "ov_s:channel": 2, "ov_c:channel": 1, "ov_b:channel": 11}));
        let b = parse(json!({"channel": 7, "ov_c:channel": 3}));
        let c = parse(json!({"channel": 13, "msg:x": 18}));
        assert_eq!(
            merge_states(&[&a, &b, &c]).unwrap(),
            merge_states(&[&c, &a, &b]).unwrap()
        );
        assert_eq!(
            merge_states(&[&merge_states(&[&a, &b]).unwrap(), &c]).unwrap(),
            merge_states(&[&a, &merge_states(&[&b, &c]).unwrap()]).unwrap()
        );
        assert_eq!(merge_states(&[&a, &a]).unwrap(), a);
        let merged = merge_states(&[&a, &b]).unwrap();
        assert!(!override_active(merged.overrides.get("channel"), Some(11)));
    }

    #[test]
    fn unknown_is_not_zero_and_children_never_advance_parents() {
        let room = advance_read(&ReadState::default(), "room", 10).unwrap();
        let read = advance_read(&room, "msg:reply", 30).unwrap();
        assert_eq!(effective_frontier(&read, "missing", None, None), None);
        assert_eq!(
            effective_frontier(&parse(json!({"room": 0})), "room", None, None),
            Some(0)
        );
        assert_eq!(
            effective_frontier(&read, "msg:other", Some("room"), None),
            Some(10)
        );
        assert_eq!(effective_frontier(&read, "room", None, None), Some(10));
        let thread = advance_read(&read, "thread:root", 20).unwrap();
        assert_eq!(
            effective_frontier(&thread, "msg:other", Some("room"), Some("root")),
            Some(20)
        );
    }

    #[test]
    fn malformed_override_groups_drop_whole_but_keep_frontiers() {
        for contexts in [
            json!({"room": 4, "ov_s:room": 3}),
            json!({"room": 4, "ov_c:room": 1, "ov_b:room": 3}),
            json!({"room": 4, "ov_s:room": 3, "ov_c:room": -1, "ov_b:room": 4}),
        ] {
            assert_eq!(parse(contexts), state(&[("room", 4)]));
        }
    }

    #[test]
    fn inactive_overrides_publish_a_permanent_clear_floor() {
        let set = change_override(&parse(json!({"room": 10})), "room", true, 10).unwrap();
        assert!(override_active(set.overrides.get("room"), Some(10)));
        let clear = advance_read(&set, "room", 11).unwrap();
        let canonical = encode_blob("test", &clear, |key| {
            effective_frontier(&clear, key, None, None)
        })
        .unwrap();
        assert_eq!(canonical["contexts"], json!({"room": 11, "ov_c:room": 1}));
        let replayed = merge_states(&[&parse_blob(&canonical).unwrap().state, &set]).unwrap();
        assert!(!override_active(replayed.overrides.get("room"), Some(10)));
        assert_eq!(
            change_override(
                &parse(json!({"ov_c:room": READ_STATE_MAX})),
                "room",
                true,
                10
            ),
            Err(ReadStateError::CounterExhausted)
        );
    }

    #[test]
    fn reserved_keys_are_escaped_once() {
        let parsed = parse(json!({"esc:ov_s:x": 2, "esc:esc:x": 3, "__proto__": 4}));
        assert_eq!(
            parsed,
            state(&[("ov_s:x", 2), ("esc:x", 3), ("__proto__", 4)])
        );
        let blob = encode_blob("test", &parsed, |_| None).unwrap();
        assert_eq!(blob["contexts"]["esc:ov_s:x"], json!(2));
        assert_eq!(blob["contexts"]["esc:esc:x"], json!(3));
        assert_eq!(blob["contexts"]["__proto__"], json!(4));
        assert_eq!(parse_blob(&blob).unwrap().state, parsed);
    }

    #[test]
    fn parsing_validates_version_capacity_bytes_and_uint32() {
        assert_eq!(
            parse_blob(&json!({"v": 2, "client_id": "a", "contexts": {}})),
            Err(ReadStateError::UnsupportedBlob)
        );
        assert_eq!(
            parse_blob(&json!({"v": 1, "client_id": "", "contexts": {}})),
            Err(ReadStateError::UnsupportedBlob)
        );
        assert_eq!(
            parse_blob(&json!({"v": 1, "client_id": "é".repeat(65), "contexts": {}})),
            Err(ReadStateError::UnsupportedBlob)
        );
        assert_eq!(
            parse(json!({
                "bad": -1, "fraction": 1.5, "huge": u64::from(READ_STATE_MAX) + 1,
                "good": 0, "whole": 3.0,
            })),
            state(&[("good", 0), ("whole", 3)])
        );
        let mut long = Map::new();
        long.insert("é".repeat(129), json!(1));
        assert!(parse(Value::Object(long)).frontiers.is_empty());
        let many: Map<String, Value> = (0..10_001).map(|n| (format!("k{n}"), json!(1))).collect();
        assert!(matches!(
            parse_blob(&json!({"v": 1, "client_id": "a", "contexts": many})),
            Err(ReadStateError::Capacity(_))
        ));
    }

    fn app_data(tags: Vec<Vec<&str>>, kind: u16) -> Event {
        EventBuilder::new(Kind::from_u16(kind), "")
            .tags(tags.into_iter().map(|tag| Tag::parse(tag).unwrap()))
            .finalize(&Keys::generate())
            .unwrap()
    }

    #[test]
    fn coordinates_need_one_d_and_one_discoverability_tag() {
        let d = format!("read-state:{}", "a".repeat(32));
        let base = vec![vec!["d", d.as_str()], vec!["t", "read-state"]];
        let event = app_data(base.clone(), 30078);
        assert_eq!(read_coordinate(&event), Some(d.as_str()));
        let mut two_d = base.clone();
        two_d.push(vec!["d", "other"]);
        assert_eq!(read_coordinate(&app_data(two_d, 30078)), None);
        let mut two_t = base.clone();
        two_t.push(vec!["t", "read-state"]);
        assert_eq!(read_coordinate(&app_data(two_t, 30078)), None);
        assert_eq!(read_coordinate(&app_data(base, 9)), None);
        let upper = format!("read-state:{}", "A".repeat(32));
        let upper = app_data(vec![vec!["d", &upper], vec!["t", "read-state"]], 30078);
        assert_eq!(read_coordinate(&upper), None);
    }

    #[test]
    fn versions_order_same_second_publications_and_stop_runaways() {
        assert_eq!(read_version(100, 100), Ok(101));
        assert_eq!(read_version(100, 110), Ok(111));
        assert_eq!(read_version(100, 160), Err(ReadStateError::ClockAhead));
    }

    #[test]
    fn targets_validate_their_keys() {
        let id = hex64(1);
        assert_eq!(ReadTarget::channel("room").key().unwrap(), "room");
        assert_eq!(
            ReadTarget::thread("room", &id).key().unwrap(),
            format!("thread:{id}")
        );
        assert_eq!(
            ReadTarget::message("room", &id).key().unwrap(),
            format!("msg:{id}")
        );
        assert!(ReadTarget::channel("a b").key().is_err());
        assert!(ReadTarget::message("room", "XYZ").key().is_err());
    }

    // -- retention (read-state-retention.test.ts) ---------------------------

    #[test]
    fn pruning_never_loses_floors_or_override_ancestry() {
        let mut input = state(&[("room", 20), ("thread:root", 30), ("msg:child", 1)]);
        for n in 0..300 {
            input.frontiers.insert(format!("msg:{n}"), 100);
        }
        let child = Override {
            set: 4,
            clear: 0,
            baseline: 15,
        };
        let other = Override {
            set: 0,
            clear: 9,
            baseline: 0,
        };
        input.overrides.insert("msg:child".into(), child);
        input.overrides.insert("other".into(), other);
        let kept = retain_read(&[&input], &BTreeMap::new(), "fixture", 512, None)
            .unwrap()
            .state;
        assert_eq!(kept.overrides, input.overrides);
        assert_eq!(kept.frontier("room"), Some(20));
        assert_eq!(kept.frontier("thread:root"), Some(30));
        assert_eq!(kept.frontier("msg:child"), Some(1));
        assert!(!override_active(
            kept.overrides.get("msg:child"),
            effective_frontier(&kept, "msg:child", Some("room"), Some("root"))
        ));
        assert!(matches!(
            retain_read(&[&input], &BTreeMap::new(), "fixture", 50, None),
            Err(ReadStateError::Capacity(_))
        ));
    }

    #[test]
    fn a_recent_read_of_old_history_outranks_event_age() {
        let mut input = state(&[("msg:old", 1)]);
        for n in 0..300 {
            input.frontiers.insert(format!("msg:{n}"), 100);
        }
        let recent = BTreeMap::from([("msg:old".to_string(), 1)]);
        let kept = retain_read(&[&input], &recent, "fixture", 128, None)
            .unwrap()
            .state;
        assert_eq!(kept.frontier("msg:old"), Some(1));
        assert!(kept.frontiers.keys().all(|key| key.starts_with("msg:")));
        let again = retain_read(&[&kept, &input], &recent, "fixture", 128, None)
            .unwrap()
            .state;
        assert_eq!(again, kept);
    }

    #[test]
    fn old_broad_marks_survive_a_flood_of_newer_message_reads() {
        let thread = format!("thread:{}", "a".repeat(64));
        let mut input = state(&[("quiet", 50), ("activity:other", 40), (thread.as_str(), 30)]);
        let mut recent = BTreeMap::new();
        for n in 0..2000 {
            let key = format!("msg:{}", hex64(n));
            input.frontiers.insert(key.clone(), 1000 + n as u32);
            recent.insert(key, 10 + n as u64);
        }
        for budget in [READ_JOURNAL_BYTES, READ_STATE_PLAINTEXT_BYTES] {
            let kept = retain_read(&[&input], &recent, "fixture", budget, None)
                .unwrap()
                .state;
            assert_eq!(kept.frontier("quiet"), Some(50));
            assert_eq!(kept.frontier("activity:other"), Some(40));
            assert_eq!(kept.frontier(&thread), Some(30));
            assert!(kept.frontiers.len() < 2003);
        }
    }

    #[test]
    fn recent_catch_up_never_pushes_out_a_quiet_channel_mark() {
        let id = |prefix: &str, n: usize| format!("{prefix}{n:08x}-0000-4000-8000-000000000000");
        let channels: Vec<String> = (0..300).map(|n| id("", n)).collect();
        let threads: Vec<String> = (0..100).map(|n| format!("thread:{}", hex64(n))).collect();
        let activity: Vec<String> = (0..600).map(|n| id("activity:", 1000 + n)).collect();
        let mut input = ReadState::default();
        let mut recent = BTreeMap::new();
        for key in &channels {
            input.frontiers.insert(key.clone(), 10);
        }
        for key in &threads {
            input.frontiers.insert(key.clone(), 20);
        }
        for (n, key) in activity.iter().enumerate() {
            input.frontiers.insert(key.clone(), 1000 + n as u32);
            recent.insert(key.clone(), 100 + n as u64);
        }
        let kept = retain_read(
            &[&input],
            &recent,
            "fixture",
            READ_STATE_PLAINTEXT_BYTES,
            None,
        )
        .unwrap()
        .state;
        let kept_activity = kept
            .frontiers
            .keys()
            .filter(|key| key.starts_with("activity:"))
            .count();
        assert!(kept_activity < activity.len());
        for key in channels.iter().chain(&threads) {
            assert_eq!(kept.frontier(key), input.frontier(key), "{key}");
        }
    }

    #[test]
    fn covered_marks_drop_unless_overrides_need_ancestry() {
        let input = state(&[("room", 20), ("msg:covered", 10), ("msg:other", 30)]);
        let covered = |key: &str, frontier: &dyn Fn(&str) -> Option<u32>| {
            (key == "msg:covered" && frontier("room").map_or(-1, i64::from) >= 10)
                .then(|| "room".to_string())
        };
        let kept = retain_read(
            &[&input],
            &BTreeMap::new(),
            "fixture",
            READ_JOURNAL_BYTES,
            Some(&covered),
        )
        .unwrap()
        .state;
        assert_eq!(
            kept.frontiers,
            state(&[("room", 20), ("msg:other", 30)]).frontiers
        );
        let mut with_override = input.clone();
        let other = Override {
            set: 0,
            clear: 9,
            baseline: 0,
        };
        with_override.overrides.insert("other".into(), other);
        let kept = retain_read(
            &[&with_override],
            &BTreeMap::new(),
            "fixture",
            READ_JOURNAL_BYTES,
            Some(&covered),
        )
        .unwrap()
        .state;
        assert_eq!(kept.frontiers, input.frontiers);
    }

    #[test]
    fn a_cover_that_replaces_a_fresh_read_inherits_its_recency() {
        let thread = |n: usize| format!("thread:{}", hex64(n));
        let cover = thread(999);
        let read = format!("thread-activity:{}3e7", "9".repeat(61));
        let mut input = ReadState::default();
        let mut recent = BTreeMap::new();
        for n in 0..20 {
            input.frontiers.insert(thread(n), 100 + n as u32);
            recent.insert(thread(n), 10 + n as u64);
        }
        input.frontiers.insert(cover.clone(), 60);
        input.frontiers.insert(read.clone(), 50);
        recent.insert(read.clone(), 1000);
        let covered = |key: &str, frontier: &dyn Fn(&str) -> Option<u32>| {
            (key == read && frontier(&cover).unwrap_or(0) >= frontier(&read).unwrap_or(0))
                .then(|| cover.clone())
        };
        let still_read = |kept: &ReadState| {
            kept.frontier(&read).unwrap_or(0) >= 50 || kept.frontier(&cover).unwrap_or(0) >= 50
        };
        // A cover that does not fit never replaces the fresh read.
        let tight = retain_read(&[&input], &recent, "fixture", 600, Some(&covered)).unwrap();
        assert!(tight.state.frontiers.len() < 10);
        assert!(still_read(&tight.state));
        // With room, the cover replaces the read and takes its use...
        let local = retain_read(
            &[&input],
            &recent,
            "fixture",
            READ_JOURNAL_BYTES,
            Some(&covered),
        )
        .unwrap();
        assert_eq!(local.state.frontier(&read), None);
        assert_eq!(local.state.frontier(&cover), Some(60));
        assert_eq!(local.recent.get(&cover), Some(&1000));
        // ...so the smaller publication budget still protects it.
        let published = retain_read(
            &[&local.state],
            &local.recent,
            "fixture",
            600,
            Some(&covered),
        )
        .unwrap();
        assert!(published.state.frontiers.len() < 10);
        assert!(still_read(&published.state));
    }

    #[test]
    fn the_reserve_is_bounded_and_deterministic() {
        for width in [64, 240] {
            let mut input = ReadState::default();
            for n in 0..8000 {
                input
                    .frontiers
                    .insert(format!("msg:{n:0width$x}"), n as u32 + 1);
            }
            let none = BTreeMap::new();
            let kept =
                retain_local_read(&[&input], &BTreeMap::new(), "fixture", &none, None).unwrap();
            assert!(kept.reserve.len() <= READ_RESERVE_KEYS);
            assert!(json_len(&kept.reserve) <= READ_RESERVE_BYTES);
            assert!(kept.reserve.len() > 1900);
            assert!(!kept.reserve.values().any(|value| *value == 1));
            for (key, value) in &kept.reserve {
                assert_eq!(input.frontier(key), Some(*value));
                assert_eq!(kept.state.frontier(key), None);
            }
            let again =
                retain_local_read(&[&kept.state], &kept.recent, "fixture", &kept.reserve, None)
                    .unwrap();
            assert_eq!(again, kept);
        }
    }

    #[test]
    fn returning_keys_keep_their_highest_reserved_value() {
        let kept = retain_local_read(
            &[&state(&[("msg:old", 5)])],
            &BTreeMap::from([("msg:old".to_string(), 1)]),
            "fixture",
            &BTreeMap::from([("msg:old".to_string(), 20), ("msg:other".to_string(), 30)]),
            None,
        )
        .unwrap();
        assert_eq!(kept.state.frontiers, state(&[("msg:old", 20)]).frontiers);
        assert_eq!(
            kept.reserve,
            BTreeMap::from([("msg:other".to_string(), 30)])
        );
    }

    #[test]
    fn reserved_override_floors_return_to_the_journal() {
        let mut input = ReadState::default();
        let direct = Override {
            set: 1,
            clear: 0,
            baseline: 10,
        };
        input.overrides.insert("msg:direct".into(), direct);
        let reserve = BTreeMap::from([
            ("msg:direct".to_string(), 20),
            ("room".to_string(), 30),
            ("thread:root".to_string(), 40),
        ]);
        let kept =
            retain_local_read(&[&input], &BTreeMap::new(), "fixture", &reserve, None).unwrap();
        assert_eq!(kept.state.frontier("msg:direct"), Some(20));
        let room = kept
            .state
            .frontier("room")
            .or(kept.reserve.get("room").copied());
        assert_eq!(room, Some(30));
    }

    // -- journal (read-state-storage.ts) ------------------------------------

    #[test]
    fn journals_round_trip_and_reject_corruption() {
        let identity = Identity::ephemeral();
        let viewer = identity.pubkey_hex();
        let mut journal = ReadJournal::new_random();
        assert!(is_slot(&journal.slot));
        assert_eq!(journal.client_id.len(), 36);
        journal.revision = 3;
        journal.state.frontiers.insert("room".into(), 5);
        journal
            .state
            .frontiers
            .insert(format!("msg:{}", hex64(1)), 5);
        journal.recent.insert("room".into(), 2);
        journal.local_unread.insert("room".into(), 3);
        journal.reserve.insert("old".into(), 1);
        journal.homes.insert(hex64(1), "room".into());
        journal.homes.insert(hex64(2), "room".into());
        let loaded = ReadJournal::from_json(&journal.to_json(), &viewer).unwrap();
        // Homes keep only the messages a kept mark names.
        assert_eq!(
            loaded.homes,
            BTreeMap::from([(hex64(1), "room".to_string())])
        );
        assert_eq!(loaded.state, journal.state);

        let corrupt = |change: &dyn Fn(&mut ReadJournal)| {
            let mut bad = journal.clone();
            change(&mut bad);
            ReadJournal::from_json(&bad.to_json(), &viewer).unwrap_err()
        };
        let is_corrupt = |error: ReadStateError| matches!(error, ReadStateError::CorruptJournal(_));
        assert!(is_corrupt(corrupt(&|j| j.slot = "x".into())));
        assert!(is_corrupt(corrupt(&|j| j.accepted_revision = 4)));
        assert!(is_corrupt(corrupt(&|j| {
            j.local_unread.insert("later".into(), 4);
        })));
        assert!(is_corrupt(corrupt(&|j| {
            j.recent.insert("missing".into(), 1);
        })));
        assert!(is_corrupt(corrupt(&|j| {
            j.reserve.insert("room".into(), 1);
        })));
        assert!(is_corrupt(corrupt(&|j| {
            j.state.frontiers.insert(String::new(), 1);
        })));
        assert!(ReadJournal::from_json("{", &viewer).is_err());

        // Pending bytes must be ours, for this slot, not after lastCreatedAt.
        let blob = json!({"v": 1, "client_id": journal.client_id, "contexts": {"room": 5}});
        let now = Timestamp::now().as_secs();
        let event = sign_read_state(&identity, &journal.slot, now as u32, &blob, now).unwrap();
        let mut pending = journal.clone();
        pending.last_created_at = now as u32;
        pending.pending = Some(PendingPublish { event, revision: 3 });
        assert!(ReadJournal::from_json(&pending.to_json(), &viewer).is_ok());
        let stranger = Keys::generate().public_key().to_hex();
        assert!(ReadJournal::from_json(&pending.to_json(), &stranger).is_err());
        pending.last_created_at -= 1;
        assert!(ReadJournal::from_json(&pending.to_json(), &viewer).is_err());
    }

    // -- codec --------------------------------------------------------------

    #[test]
    fn the_codec_round_trips_and_refuses_skew_and_strangers() {
        let identity = Identity::ephemeral();
        let slot = "b".repeat(32);
        let blob = json!({"v": 1, "client_id": "c", "contexts": {"room": 7}});
        assert_eq!(
            sign_read_state(&identity, &slot, 1000, &blob, 1061),
            Err(ReadStateError::InvalidSigningIntent)
        );
        assert_eq!(
            sign_read_state(&identity, "short", 1000, &blob, 1000),
            Err(ReadStateError::InvalidSigningIntent)
        );
        let event = sign_read_state(&identity, &slot, 1000, &blob, 1060).unwrap();
        let tags: Vec<Vec<String>> = event.tags.iter().map(|t| t.as_slice().to_vec()).collect();
        assert_eq!(
            tags,
            vec![
                vec!["d".to_string(), format!("read-state:{slot}")],
                vec!["t".to_string(), "read-state".to_string()]
            ]
        );
        assert_eq!(
            decode_read_state(&identity, &event).unwrap().state,
            state(&[("room", 7)])
        );
        assert_eq!(
            decode_read_state(&Identity::ephemeral(), &event),
            Err(ReadStateError::InvalidEvent)
        );
    }

    #[test]
    fn snapshot_capability_and_response_are_strict() {
        let community = "0b5c3f1e-1111-4222-8333-444455556666";
        let nip11 = json!({"read_state_snapshot": {
            "version": 1, "community_id": community, "max_events": 4096,
        }});
        assert_eq!(
            SnapshotDescriptor::from_nip11(&nip11),
            Some(SnapshotDescriptor {
                community_id: community.into()
            })
        );
        let v2 = json!({"read_state_snapshot": {"version": 2, "community_id": community}});
        assert_eq!(SnapshotDescriptor::from_nip11(&v2), None);
        let bad = json!({"read_state_snapshot": {"version": 1, "community_id": "nope"}});
        assert_eq!(SnapshotDescriptor::from_nip11(&bad), None);

        let identity = Identity::ephemeral();
        let viewer = identity.pubkey_hex();
        let body: Vec<Value> = snapshot_filter(&viewer)
            .into_iter()
            .map(Value::Object)
            .collect();
        assert_eq!(
            Value::Array(body),
            json!([{"kinds": [30078], "authors": [viewer], "read_state_snapshot": 1}])
        );
        let blob = json!({"v": 1, "client_id": "c", "contexts": {}});
        let now = Timestamp::now().as_secs();
        let event = sign_read_state(&identity, &"c".repeat(32), now as u32, &blob, now).unwrap();
        let response = |events: Vec<&Event>| {
            json!({
                "read_state_snapshot": 1, "complete": true, "community_id": community,
                "pubkey": viewer, "snapshot_id": hex64(7), "events": events,
            })
        };
        let verifier = Verifier::default();
        let one = parse_snapshot_response(&response(vec![&event]), &viewer, community, &verifier);
        assert_eq!(one.unwrap().len(), 1);
        let twice = parse_snapshot_response(
            &response(vec![&event, &event]),
            &viewer,
            community,
            &verifier,
        );
        assert_eq!(
            twice.unwrap_err(),
            ReadStateError::Snapshot("Snapshot contains duplicate coordinates")
        );
        let other = parse_snapshot_response(&response(vec![&event]), &viewer, "other", &verifier);
        assert_eq!(other.unwrap_err(), ReadStateError::SnapshotMismatch);
    }

    #[test]
    fn publish_refusals_map_only_the_exact_timestamp_reason() {
        let refusal = json!({"error": TIMESTAMP_REFUSAL});
        let unsent = json!({"error": TIMESTAMP_REFUSAL, "sent": false});
        assert_eq!(
            PublishFailure::from_response(400, &refusal),
            PublishFailure::TimestampRejected
        );
        assert_eq!(
            PublishFailure::from_response(503, &unsent),
            PublishFailure::TimestampRejected
        );
        assert!(matches!(
            PublishFailure::from_response(503, &refusal),
            PublishFailure::Other(_)
        ));
        assert!(matches!(
            PublishFailure::from_response(400, &json!({"error": "invalid: other"})),
            PublishFailure::Other(_)
        ));
    }

    // -- sync state machine (read-state.test.ts) ----------------------------

    /// A relay holding one replaceable event per coordinate.
    #[derive(Default)]
    struct Relay {
        stored: BTreeMap<String, Event>,
        failures: VecDeque<PublishFailure>,
        publishes: Vec<Event>,
    }

    impl Relay {
        fn events(&self) -> Vec<Verified> {
            let verifier = Verifier::default();
            self.stored
                .values()
                .map(|event| verifier.verify(event.clone()).unwrap())
                .collect()
        }

        fn put(&mut self, event: Event) {
            let coordinate = read_coordinate(&event).unwrap().to_owned();
            self.stored.insert(coordinate, event);
        }

        /// Runs every command the state machine asks for.
        fn drive(
            &mut self,
            sync: &mut ReadStateSync<MemoryJournal>,
            identity: &Identity,
            mut command: Option<PublishCommand>,
            now_ms: u64,
        ) {
            while let Some(next) = command.take() {
                command = match next {
                    PublishCommand::Publish(event) => {
                        self.publishes.push((*event).clone());
                        match self.failures.pop_front() {
                            Some(failure) => sync.published(Err(failure), now_ms),
                            None => {
                                self.put(*event);
                                sync.published(Ok(()), now_ms)
                            }
                        }
                    }
                    PublishCommand::ReadBack(_) => {
                        sync.read_back(Ok(self.events()), identity, None, now_ms)
                    }
                };
            }
        }
    }

    fn open(identity: &Identity, store: MemoryJournal) -> ReadStateSync<MemoryJournal> {
        ReadStateSync::open(identity.pubkey_hex(), ReadCapability::FrontierSync, store)
    }

    fn signed(identity: &Identity, slot: &str, contexts: Value) -> Verified {
        let blob = json!({"v": 1, "client_id": "peer", "contexts": contexts});
        let event = sign_read_state(identity, slot, 100, &blob, 100).unwrap();
        Verifier::default().verify(event).unwrap()
    }

    const T: u64 = 100_000;

    #[test]
    fn saves_intent_and_signed_bytes_before_publishing_then_confirms() {
        let identity = Identity::ephemeral();
        let mut sync = open(&identity, MemoryJournal::default());
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Local);
        let result = sync
            .apply(ReadUpdate::observe("room", 12), None, T)
            .unwrap();
        assert_eq!(result.sync, SyncKind::Pending);
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Pending);
        let mut relay = Relay::default();
        let command = sync.flush();
        assert!(matches!(command, Some(PublishCommand::ReadBack(_))));
        let command = sync.read_back(Ok(Vec::new()), &identity, None, T);
        let Some(PublishCommand::Publish(event)) = &command else {
            panic!("expected a publish, got {command:?}");
        };
        // The exact bytes are durable before they are sent.
        let journal = sync.journal().unwrap();
        assert_eq!(journal.pending.as_ref().unwrap().event, **event);
        assert_eq!(journal.state.frontier("room"), Some(12));
        relay.drive(&mut sync, &identity, command, T);
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Reconciled);
        let journal = sync.journal().unwrap();
        assert!(journal.pending.is_none());
        assert_eq!(journal.accepted_revision, journal.revision);
        assert_eq!(relay.publishes.len(), 1);
    }

    #[test]
    fn a_durability_failure_saves_nothing_and_publishes_nothing() {
        let identity = Identity::ephemeral();
        let mut store = open(&identity, MemoryJournal::default()).into_store();
        store.fail_stores = 1;
        let mut sync = open(&identity, store);
        let error = sync
            .apply(ReadUpdate::observe("room", 12), None, T)
            .unwrap_err();
        assert_eq!(error, ReadStateError::Storage("disk full".into()));
        assert_eq!(sync.state().frontier("room"), None);
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Error);
        assert_eq!(sync.deadline(), None);
        assert_eq!(sync.flush(), None);
    }

    #[test]
    fn the_debounce_fires_once_after_five_seconds_without_resetting() {
        let identity = Identity::ephemeral();
        let mut sync = open(&identity, MemoryJournal::default());
        sync.apply(ReadUpdate::observe("room", 12), None, T)
            .unwrap();
        assert_eq!(sync.deadline(), Some(T + PUBLISH_DEBOUNCE_MS));
        sync.apply(ReadUpdate::observe("room", 13), None, T + 4000)
            .unwrap();
        assert_eq!(sync.deadline(), Some(T + PUBLISH_DEBOUNCE_MS));
        assert_eq!(sync.poll(T + 4999), None);
        assert!(matches!(
            sync.poll(T + 5000),
            Some(PublishCommand::ReadBack(_))
        ));
        assert_eq!(sync.deadline(), None);
        // No second flush while one runs.
        assert_eq!(sync.flush(), None);
    }

    #[test]
    fn an_unknown_publish_retries_the_same_bytes_before_newer_intent() {
        let identity = Identity::ephemeral();
        let mut relay = Relay::default();
        let mut first = open(&identity, MemoryJournal::default());
        first
            .apply(ReadUpdate::observe("room", 12), None, T)
            .unwrap();
        relay
            .failures
            .push_back(PublishFailure::Other("response lost".into()));
        let command = first.flush();
        relay.drive(&mut first, &identity, command, T);
        assert_eq!(first.snapshot().status, ReadSyncStatus::Error);
        let unknown = first.journal().unwrap().pending.clone().unwrap().event;
        // A failed revision waits for an explicit retry.
        assert_eq!(first.deadline(), None);

        // Restart: the saved bytes come back and nothing publishes by itself.
        let mut second = open(&identity, first.into_store());
        assert_eq!(second.snapshot().status, ReadSyncStatus::Pending);
        assert_eq!(second.deadline(), None);
        second
            .apply(ReadUpdate::observe("room", 13), None, T)
            .unwrap();
        let command = second.flush();
        relay.drive(&mut second, &identity, command, T);
        assert_eq!(relay.publishes[1], unknown);
        assert_eq!(relay.publishes.len(), 3);
        assert_eq!(second.snapshot().status, ReadSyncStatus::Reconciled);
        assert_eq!(second.journal().unwrap().last_created_at, 101);
        assert!(second.journal().unwrap().pending.is_none());
    }

    #[test]
    fn a_repeated_timestamp_refusal_gets_one_replacement_per_flush() {
        let identity = Identity::ephemeral();
        let mut relay = Relay::default();
        let mut sync = open(&identity, MemoryJournal::default());
        sync.apply(ReadUpdate::observe("room", 12), None, T)
            .unwrap();
        relay
            .failures
            .push_back(PublishFailure::Other("response lost".into()));
        let command = sync.flush();
        relay.drive(&mut sync, &identity, command, T);
        let later = 1_001_000;
        relay.failures.push_back(PublishFailure::TimestampRejected);
        relay.failures.push_back(PublishFailure::TimestampRejected);
        let command = sync.flush();
        relay.drive(&mut sync, &identity, command, later);
        assert_eq!(relay.publishes.len(), 3);
        let journal = sync.journal().unwrap();
        let pending = journal.pending.as_ref().unwrap();
        assert_eq!(pending.event.created_at.as_secs(), 1001);
        assert_eq!(journal.accepted_revision, 0);
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Error);
        assert_eq!(sync.poll(later + 120_000), None);
    }

    #[test]
    fn a_recent_timestamp_refusal_is_not_renewed() {
        let identity = Identity::ephemeral();
        let mut relay = Relay::default();
        let mut sync = open(&identity, MemoryJournal::default());
        sync.apply(ReadUpdate::observe("room", 12), None, T)
            .unwrap();
        relay
            .failures
            .push_back(PublishFailure::Other("lost".into()));
        let command = sync.flush();
        relay.drive(&mut sync, &identity, command, T);
        let pending = sync.journal().unwrap().pending.clone();
        relay.failures.push_back(PublishFailure::TimestampRejected);
        let command = sync.flush();
        relay.drive(&mut sync, &identity, command, T + 30_000);
        assert_eq!(sync.journal().unwrap().pending, pending);
        assert_eq!(relay.publishes.len(), 2);
    }

    #[test]
    fn a_conflicting_slot_client_blocks_recovery() {
        let identity = Identity::ephemeral();
        let mut relay = Relay::default();
        let mut sync = open(&identity, MemoryJournal::default());
        sync.apply(ReadUpdate::observe("room", 12), None, T)
            .unwrap();
        relay
            .failures
            .push_back(PublishFailure::Other("lost".into()));
        let command = sync.flush();
        relay.drive(&mut sync, &identity, command, T);
        let saved = sync.journal().unwrap().clone();
        let blob = json!({"v": 1, "client_id": "another-client", "contexts": {"room": 13}});
        relay.put(sign_read_state(&identity, &saved.slot, 1001, &blob, 1001).unwrap());
        relay.failures.push_back(PublishFailure::TimestampRejected);
        let command = sync.flush();
        relay.drive(&mut sync, &identity, command, 1_001_000);
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Error);
        let error = sync.snapshot().error.clone().unwrap();
        assert!(error.contains("slot conflict"), "{error}");
        assert_eq!(sync.journal().unwrap().pending, saved.pending);
        assert_eq!(sync.journal().unwrap().accepted_revision, 0);
    }

    #[test]
    fn an_accepted_but_unobserved_publish_keeps_its_bytes() {
        let identity = Identity::ephemeral();
        let mut sync = open(&identity, MemoryJournal::default());
        sync.apply(ReadUpdate::observe("room", 12), None, T)
            .unwrap();
        assert!(sync.flush().is_some());
        let command = sync.read_back(Ok(Vec::new()), &identity, None, T);
        assert!(matches!(command, Some(PublishCommand::Publish(_))));
        // The relay acknowledges but the readback does not show it.
        let command = sync.published(Ok(()), T);
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Accepted);
        assert!(matches!(command, Some(PublishCommand::ReadBack(_))));
        assert_eq!(sync.read_back(Ok(Vec::new()), &identity, None, T), None);
        let error = sync.snapshot().error.clone().unwrap();
        assert!(error.contains("observation"), "{error}");
        assert!(sync.journal().unwrap().pending.is_some());
    }

    #[test]
    fn local_manual_intent_neither_lowers_frontiers_nor_publishes() {
        let identity = Identity::ephemeral();
        let mut sync = open(&identity, MemoryJournal::default());
        let result = sync.apply(ReadUpdate::mark_local("room"), None, T).unwrap();
        assert_eq!(result.sync, SyncKind::LocalOnly);
        let journal = sync.journal().unwrap();
        assert_eq!(journal.local_unread.get("room"), Some(&1));
        assert_eq!(journal.accepted_revision, journal.revision);
        assert_eq!(sync.deadline(), None);
        // An explicit read clears it; an automatic one does not.
        sync.apply(ReadUpdate::observe("room", 5), None, T).unwrap();
        assert!(sync.local_unread("room").is_some());
        sync.apply(ReadUpdate::read("room", 6, Vec::new()), None, T)
            .unwrap();
        assert_eq!(sync.local_unread("room"), None);
        assert_eq!(sync.state().frontier("room"), Some(6));
    }

    #[test]
    fn a_newer_manual_unread_fences_an_older_lease() {
        let identity = Identity::ephemeral();
        let mut sync = open(&identity, MemoryJournal::default());
        let lease_revision = sync.revision();
        sync.apply(ReadUpdate::mark_local("room"), None, T).unwrap();
        let update = ReadUpdate::observe("activity:room", 9).fenced("room", lease_revision);
        assert_eq!(sync.apply(update, None, T), Err(ReadStateError::Expired));
        assert_eq!(sync.state().frontier("activity:room"), None);
        // An expected rejection is not a sync failure.
        assert_ne!(sync.snapshot().status, ReadSyncStatus::Error);
    }

    #[test]
    fn read_only_hosts_keep_local_intent_but_never_write_frontiers() {
        let identity = Identity::ephemeral();
        let mut sync = ReadStateSync::open(
            identity.pubkey_hex(),
            ReadCapability::ReadOnly,
            MemoryJournal::default(),
        );
        assert_eq!(
            sync.apply(ReadUpdate::observe("room", 1), None, T),
            Err(ReadStateError::SyncUnsupported)
        );
        let key = format!("msg:{}", hex64(1));
        sync.apply(ReadUpdate::mark_local(key.clone()), None, T)
            .unwrap();
        let read = MessageRead {
            key,
            timestamp: 5,
            channel_id: "room".into(),
            root_id: None,
        };
        // Not yet read: a read-only host cannot record it.
        assert_eq!(
            sync.apply(ReadUpdate::messages(vec![read], None), None, T),
            Err(ReadStateError::SyncUnsupported)
        );
        assert_eq!(sync.flush(), None);
    }

    #[test]
    fn peers_merge_without_bumping_the_revision() {
        let identity = Identity::ephemeral();
        let mut sync = open(&identity, MemoryJournal::default());
        let peer = signed(&identity, &"d".repeat(32), json!({"room": 30}));
        assert!(sync.accept(&[peer], &identity, None));
        assert_eq!(sync.state().frontier("room"), Some(30));
        assert_eq!(sync.revision(), 0);
        assert_eq!(sync.deadline(), None);
        // Other kind-30078 contracts are not read state.
        let other = Verifier::default()
            .verify(app_data(vec![vec!["d", "channel-mutes"]], 30078))
            .unwrap();
        assert!(!sync.accept(&[other], &identity, None));
    }

    /// §9.3 #9: a peer's override register does not block publication; our
    /// slot publishes frontiers only and keeps the inherited ancestry.
    #[test]
    fn foreign_overrides_do_not_block_our_publication() {
        let identity = Identity::ephemeral();
        let mut relay = Relay::default();
        let mut sync = open(&identity, MemoryJournal::default());
        let thread = format!("thread:{}", hex64(9));
        let message = format!("msg:{}", hex64(8));
        let mut contexts = Map::new();
        contexts.insert("room".into(), json!(5));
        contexts.insert(thread.clone(), json!(7));
        contexts.insert(format!("ov_s:{message}"), json!(2));
        contexts.insert(format!("ov_c:{message}"), json!(0));
        contexts.insert(format!("ov_b:{message}"), json!(10));
        let peer = signed(&identity, &"e".repeat(32), Value::Object(contexts));
        relay.put(peer.event().clone());
        assert!(sync.accept(&relay.events(), &identity, None));
        assert!(sync.state().overrides.contains_key(&message));
        sync.apply(ReadUpdate::observe("other", 20), None, T)
            .unwrap();
        let command = sync.flush();
        relay.drive(&mut sync, &identity, command, T);
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Reconciled);
        let coordinate = sync.journal().unwrap().coordinate();
        let ours = relay.stored.get(&coordinate).unwrap();
        let published = decode_read_state(&identity, ours).unwrap().state;
        assert!(published.overrides.is_empty());
        assert_eq!(published.frontier("room"), Some(5));
        assert_eq!(published.frontier(&thread), Some(7));
        assert_eq!(published.frontier("other"), Some(20));
    }

    #[test]
    fn refresh_reports_completeness_and_ignores_stale_results() {
        let identity = Identity::ephemeral();
        let mut sync = open(&identity, MemoryJournal::default());
        assert!(sync.needs_refresh());
        let descriptor = SnapshotDescriptor {
            community_id: "0b5c3f1e-1111-4222-8333-444455556666".into(),
        };
        let request = sync.begin_refresh(Some(&descriptor)).unwrap();
        assert!(request.complete);
        assert_eq!(sync.begin_refresh(None), None);
        sync.finish_refresh(request, Ok(Vec::new()), &identity, None, T);
        assert_eq!(sync.snapshot().completeness, Completeness::Snapshot);
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Reconciled);

        let request = sync.begin_refresh(None).unwrap();
        assert!(!request.complete);
        sync.stale();
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Stale);
        let late = signed(&identity, &"f".repeat(32), json!({"late": 1}));
        sync.finish_refresh(request, Ok(vec![late]), &identity, None, T);
        assert_eq!(sync.state().frontier("late"), None);
        assert_eq!(sync.snapshot().completeness, Completeness::Unknown);
    }

    #[test]
    fn a_saved_pending_publish_is_scheduled_by_the_first_refresh() {
        let identity = Identity::ephemeral();
        let mut relay = Relay::default();
        let mut first = open(&identity, MemoryJournal::default());
        first
            .apply(ReadUpdate::observe("room", 12), None, T)
            .unwrap();
        relay
            .failures
            .push_back(PublishFailure::Other("lost".into()));
        let command = first.flush();
        relay.drive(&mut first, &identity, command, T);
        let mut second = open(&identity, first.into_store());
        assert_eq!(second.deadline(), None);
        let request = second.begin_refresh(None).unwrap();
        second.finish_refresh(request, Ok(relay.events()), &identity, None, T);
        assert_eq!(second.snapshot().status, ReadSyncStatus::Pending);
        assert_eq!(second.deadline(), Some(T + PUBLISH_DEBOUNCE_MS));
    }

    #[test]
    fn a_corrupt_journal_is_an_error_and_never_overwritten() {
        let identity = Identity::ephemeral();
        let store = MemoryJournal {
            record: Some("{\"version\":1}".into()),
            fail_stores: 0,
        };
        let mut sync = open(&identity, store);
        assert_eq!(sync.snapshot().status, ReadSyncStatus::Error);
        assert_eq!(
            sync.apply(ReadUpdate::mark_local("room"), None, T),
            Err(ReadStateError::Unavailable)
        );
        assert_eq!(sync.into_store().record.as_deref(), Some("{\"version\":1}"));
    }
}
