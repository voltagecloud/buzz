//! NIP-AT turn transcripts (kind 44201).
//!
//! The harness records one turn's ACP traffic from the same raw JSON-RPC
//! reads and writes the observer bus sees, keeps the `session/update` items a
//! transcript carries, and at turn end publishes them as encrypted parts to
//! the owner and to each live owner-delegated gateway. Publishing is best
//! effort, like turn metrics: a failure is logged and never fails the turn.
//!
//! Every buffer is bounded: string leaves over [`LEAF_LIMIT_BYTES`] are elided
//! while recording, one item is shrunk to [`ITEM_LIMIT_BYTES`], coalesced
//! chunk text keeps only the elided head and tail, and the recorder stops at
//! [`MAX_RECORDED_UPDATES`] and [`HARD_RECORDED_BYTES`], counting what it
//! drops in the payload's `omitted`. Known secret values are redacted before
//! anything is recorded ([`Secrets`]). See `docs/nips/NIP-AT.md`.

mod secrets;

pub(crate) use secrets::Secrets;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use buzz_sdk::transcript::{
    build_agent_session_transcript, encrypt_part, valid_github_repo, ContextWindow,
    TranscriptEnvelope, TranscriptPayload, TranscriptUsage, WorkspaceEntry, MAX_PARTS,
    MAX_PART_PLAINTEXT_BYTES, MAX_WORKSPACE_ENTRIES, PAYLOAD_VERSION,
};
use nostr::{EventId, PublicKey};
use serde_json::{json, Value};
use tokio_util::task::TaskTracker;

use crate::gateway::GatewayAccess;
use crate::relay::RestClient;

/// String leaves longer than this are elided with the observer-frame elision.
pub(crate) const LEAF_LIMIT_BYTES: usize = 8 * 1024;
/// Most updates one turn records.
pub(crate) const MAX_RECORDED_UPDATES: usize = 2048;
/// Past this many recorded bytes, a new tool update keeps only its skeleton.
const SOFT_RECORDED_BYTES: usize = 1024 * 1024;
/// Past this many recorded bytes, new updates are dropped and counted.
pub(crate) const HARD_RECORDED_BYTES: usize = 2 * 1024 * 1024;
/// Coalesced chunk text is compacted to its elided head and tail past this.
const CHUNK_COMPACT_BYTES: usize = 2 * LEAF_LIMIT_BYTES;
/// Bound on reading git checkouts at turn end.
const WORKSPACE_TIMEOUT: Duration = Duration::from_secs(2);
/// Directory entries scanned for git checkouts.
const MAX_SCANNED_ENTRIES: usize = 4096;
/// Bound on one git command's captured stdout.
const MAX_GIT_OUTPUT_BYTES: usize = 4096;
/// Most bytes one recorded item keeps; a larger item is shrunk alone, so it
/// can never spend the recording budget the other items need. No larger item
/// could fit a part anyway.
pub(crate) const ITEM_LIMIT_BYTES: usize = MAX_PART_PLAINTEXT_BYTES;
/// Bound on one part's relay submission attempt. It leaves room for the REST
/// client's own transient-status retries.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Submission attempts for one part.
const SUBMIT_ATTEMPTS: u32 = 4;
/// Delay before the second attempt; it doubles for each later attempt.
const SUBMIT_RETRY_BASE: Duration = Duration::from_millis(250);
/// Bound on publishing a whole turn, retries included.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(30);
/// Fields of a tool update a transcript never elides.
const TOOL_SKELETON_FIELDS: [&str; 6] = [
    "sessionUpdate",
    "toolCallId",
    "title",
    "kind",
    "status",
    "locations",
];
/// Tool fields elided first, oldest item first: outputs and adapter metadata
/// (for example claude-agent-acp's `_meta.claudeCode.toolResponse`). Every
/// other non-skeleton field, such as `rawInput`, goes in a second pass.
const TOOL_OUTPUT_FIELDS: [&str; 3] = ["content", "rawOutput", "_meta"];
/// Most `locations` entries a tool skeleton keeps.
pub(crate) const MAX_SKELETON_LOCATIONS: usize = 32;
/// Most serialized bytes of `locations` a tool skeleton keeps.
const MAX_SKELETON_LOCATION_BYTES: usize = 8 * 1024;

/// Turn sequence for this harness process.
static TURN_SEQ: AtomicU64 = AtomicU64::new(0);

/// A turn's recorder, shared by the ACP client that feeds it and the guard
/// that publishes it.
pub(crate) type SharedRecorder = Arc<Mutex<TurnRecorder>>;

/// The `session/update` items and prompt outcome of one turn.
#[derive(Default)]
pub(crate) struct TurnRecorder {
    secrets: Arc<Secrets>,
    entries: Vec<Entry>,
    bytes: usize,
    omitted: u64,
    prompt_id: Option<Value>,
    /// The newest prompt's session: only its updates belong to the turn.
    session_id: Option<String>,
    prompted: bool,
    stop_reason: Option<String>,
    usage: Option<TranscriptUsage>,
    context: Option<ContextWindow>,
    closed: bool,
}

enum Entry {
    Update(Value),
    Chunk(ChunkRun),
}

/// Consecutive text chunks of one kind and message, coalesced.
struct ChunkRun {
    kind: String,
    message_id: Option<String>,
    /// The first chunk, whose `content.text` is replaced at finish.
    update: Value,
    text: RollingText,
}

/// Text that keeps only what the observer elision would keep: everything
/// while short, and otherwise its head, its tail, and the removed byte count.
#[derive(Default)]
struct RollingText {
    buf: String,
    /// Length of the retained head once compacted.
    head_len: usize,
    /// Bytes already removed from between head and tail.
    removed: usize,
}

impl RollingText {
    /// Append `text`, redacting any secret it completes before compaction can
    /// cut through one.
    fn push(&mut self, text: &str, secrets: &Secrets) {
        let appended = self.buf.len();
        self.buf.push_str(text);
        // The retained head is final once compacted; never rewrite it.
        let floor = if self.removed > 0 { self.head_len } else { 0 };
        secrets.redact_appended(&mut self.buf, appended, floor);
        if self.buf.len() > CHUNK_COMPACT_BYTES {
            self.compact();
        }
    }

    fn compact(&mut self) {
        if self.removed == 0 {
            self.head_len = crate::floor_char_boundary(
                &self.buf,
                crate::OBSERVER_LEAF_RETAIN_BYTES.min(self.buf.len()),
            );
        }
        let tail_start = self.tail_start();
        if tail_start > self.head_len {
            self.removed += tail_start - self.head_len;
            self.buf.replace_range(self.head_len..tail_start, "");
        }
    }

    fn tail_start(&self) -> usize {
        crate::ceil_char_boundary(
            &self.buf,
            self.buf
                .len()
                .saturating_sub(crate::OBSERVER_LEAF_RETAIN_BYTES),
        )
        .max(self.head_len)
    }

    /// The final text, elided exactly as `elide_leaf` would elide the whole
    /// string when it exceeds [`LEAF_LIMIT_BYTES`].
    fn finish(mut self) -> String {
        if self.removed == 0 {
            let mut value = Value::String(self.buf);
            elide_large_leaves(&mut value);
            return match value {
                Value::String(text) => text,
                _ => String::new(),
            };
        }
        let tail_start = self.tail_start();
        let removed = self.removed + (tail_start - self.head_len);
        let mut text = String::with_capacity(self.buf.len() + 32);
        text.push_str(&self.buf[..self.head_len]);
        text.push_str(&crate::elision_marker(removed));
        text.push_str(&self.buf[tail_start..]);
        self.buf.clear();
        text
    }

    fn retained(&self) -> usize {
        self.buf.len()
    }
}

/// Elide every string leaf over [`LEAF_LIMIT_BYTES`] with the observer-frame
/// elision (head and tail kept, middle replaced by a byte-count marker).
pub(crate) fn elide_large_leaves(value: &mut Value) {
    match value {
        Value::String(text) if text.len() > LEAF_LIMIT_BYTES && crate::leaf_shrinks(text) => {
            crate::elide_leaf(value);
        }
        Value::Array(items) => items.iter_mut().for_each(elide_large_leaves),
        Value::Object(map) => map.values_mut().for_each(elide_large_leaves),
        _ => {}
    }
}

fn json_len(value: &Value) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len())
}

fn is_tool_update(update: &Value) -> bool {
    matches!(
        update.get("sessionUpdate").and_then(Value::as_str),
        Some("tool_call" | "tool_call_update")
    )
}

/// A tool update reduced to the fields a transcript never elides, with
/// `locations` cut to [`MAX_SKELETON_LOCATIONS`] entries and
/// [`MAX_SKELETON_LOCATION_BYTES`].
fn tool_skeleton(update: &Value) -> Value {
    let mut skeleton = serde_json::Map::new();
    for field in TOOL_SKELETON_FIELDS {
        let Some(value) = update.get(field) else {
            continue;
        };
        if field != "locations" {
            skeleton.insert(field.to_owned(), value.clone());
        } else if let Some(locations) = value.as_array() {
            let mut kept = Vec::new();
            let mut bytes = 0;
            for location in locations.iter().take(MAX_SKELETON_LOCATIONS) {
                bytes += json_len(location) + 1;
                if bytes > MAX_SKELETON_LOCATION_BYTES {
                    break;
                }
                kept.push(location.clone());
            }
            skeleton.insert(field.to_owned(), Value::Array(kept));
        }
    }
    Value::Object(skeleton)
}

/// A chunk reduced to its kind, message id, and text: what a reader shows.
fn chunk_skeleton(update: &Value) -> Value {
    let mut skeleton = serde_json::Map::new();
    for field in ["sessionUpdate", "messageId"] {
        if let Some(value) = update.get(field).filter(|value| value.is_string()) {
            skeleton.insert(field.to_owned(), value.clone());
        }
    }
    if let Some(text) = update
        .pointer("/content/text")
        .filter(|text| text.is_string())
    {
        skeleton.insert("content".into(), json!({"type": "text", "text": text}));
    }
    Value::Object(skeleton)
}

fn is_chunk(update: &Value) -> bool {
    matches!(
        update.get("sessionUpdate").and_then(Value::as_str),
        Some("agent_message_chunk" | "agent_thought_chunk")
    )
}

/// A tool update's non-skeleton fields: its outputs and `_meta`
/// ([`TOOL_OUTPUT_FIELDS`]) when `outputs`, else every other one, `rawInput`
/// first.
fn tool_fields(update: &Value, outputs: bool) -> Vec<String> {
    let Some(map) = update.as_object() else {
        return Vec::new();
    };
    if outputs {
        return TOOL_OUTPUT_FIELDS
            .iter()
            .filter(|field| map.contains_key(**field))
            .map(|field| (*field).to_owned())
            .collect();
    }
    let mut fields: Vec<String> = map
        .keys()
        .filter(|field| {
            !TOOL_SKELETON_FIELDS.contains(&field.as_str())
                && !TOOL_OUTPUT_FIELDS.contains(&field.as_str())
        })
        .cloned()
        .collect();
    fields.sort_by_key(|field| field != "rawInput");
    fields
}

/// Replace a tool update's `field` with an elision marker, but only when that
/// makes it smaller. Returns whether the field changed.
fn elide_tool_field(update: &mut Value, field: &str) -> bool {
    let Some(map) = update.as_object_mut() else {
        return false;
    };
    let Some(removed) = map.get(field) else {
        return false;
    };
    if is_elided_field(removed) {
        return false;
    }
    let removed_len = json_len(removed);
    let marker = crate::elision_marker(removed_len);
    let replacement = if field == "content" {
        json!([{"type": "content", "content": {"type": "text", "text": marker}}])
    } else {
        Value::String(marker)
    };
    if json_len(&replacement) >= removed_len {
        return false;
    }
    map.insert(field.to_owned(), replacement);
    true
}

/// Elide a tool update's outputs and `_meta` (`outputs`), or its other
/// non-skeleton fields. Returns whether any field changed.
fn elide_tool_fields(update: &mut Value, outputs: bool) -> bool {
    let mut changed = false;
    for field in tool_fields(update, outputs) {
        changed |= elide_tool_field(update, &field);
    }
    changed
}

/// Shrink one item to at most `limit` serialized bytes on its own, so it
/// cannot crowd out the others. A tool item loses its outputs and `_meta`,
/// then its other non-skeleton fields, one at a time, and then becomes its
/// skeleton; a chunk becomes its skeleton. Returns whether the item fits. One
/// that does not must be dropped and counted.
fn shrink_item(update: &mut Value, limit: usize) -> bool {
    if json_len(update) <= limit {
        return true;
    }
    if is_tool_update(update) {
        for outputs in [true, false] {
            for field in tool_fields(update, outputs) {
                if elide_tool_field(update, &field) && json_len(update) <= limit {
                    return true;
                }
            }
        }
        *update = tool_skeleton(update);
    } else if is_chunk(update) {
        *update = chunk_skeleton(update);
    }
    json_len(update) <= limit
}

/// Whether a tool field already holds only an elision marker, so it is never
/// elided twice (which would misreport the removed byte count).
fn is_elided_field(value: &Value) -> bool {
    let marker = match value {
        Value::String(text) => text.as_str(),
        Value::Array(items) if items.len() == 1 => items[0]
            .pointer("/content/text")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        _ => "",
    };
    marker.starts_with("…[elided ") && marker.ends_with(" bytes]…")
}

impl TurnRecorder {
    /// A recorder that redacts `secrets` from everything it records.
    pub(crate) fn new(secrets: Arc<Secrets>) -> Self {
        Self {
            secrets,
            ..Self::default()
        }
    }

    /// Record one JSON-RPC message the harness wrote to the agent.
    pub(crate) fn record_write(&mut self, message: &Value) {
        if self.closed || message.get("method").and_then(Value::as_str) != Some("session/prompt") {
            return;
        }
        // Fence the outcome to the newest prompt: a setup prompt earlier in
        // the same turn must not lend the final prompt its stop reason.
        self.prompt_id = message.get("id").cloned();
        self.session_id = message
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.prompted = true;
        self.stop_reason = None;
        self.usage = None;
    }

    /// Record one JSON-RPC message the agent sent to the harness.
    pub(crate) fn record_read(&mut self, message: &Value) {
        if self.closed {
            return;
        }
        if message.get("method").and_then(Value::as_str) == Some("session/update") {
            // Only the prompted session's updates are this turn's work: not
            // another session's, and not a `session/load` history replay
            // before the prompt.
            let session = message.pointer("/params/sessionId").and_then(Value::as_str);
            if session.is_none() || session != self.session_id.as_deref() {
                return;
            }
            if let Some(update) = message.pointer("/params/update") {
                self.record_update(update);
            }
            return;
        }
        if message.get("method").is_some()
            || self.prompt_id.is_none()
            || message.get("id") != self.prompt_id.as_ref()
        {
            return;
        }
        if message.get("error").is_some() {
            self.stop_reason = Some("error".into());
            return;
        }
        self.stop_reason = message
            .pointer("/result/stopReason")
            .and_then(Value::as_str)
            .map(|reason| {
                if reason.len() <= 32 && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                {
                    reason.to_owned()
                } else {
                    "unknown".to_owned()
                }
            });
        self.usage = message.pointer("/result/usage").and_then(parse_usage);
    }

    fn record_update(&mut self, update: &Value) {
        let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
            return;
        };
        if kind == "usage_update" {
            if let (Some(size), Some(used)) = (
                update.get("size").and_then(Value::as_u64),
                update.get("used").and_then(Value::as_u64),
            ) {
                self.context = Some(ContextWindow { size, used });
            }
            return;
        }
        if !buzz_sdk::transcript::is_kept_update(update) {
            return;
        }
        if matches!(kind, "agent_message_chunk" | "agent_thought_chunk") {
            if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
                self.record_chunk(kind, update, text);
                return;
            }
        }
        let mut update = update.clone();
        self.secrets.redact(&mut update);
        elide_large_leaves(&mut update);
        if self.bytes + json_len(&update) > SOFT_RECORDED_BYTES && is_tool_update(&update) {
            update = tool_skeleton(&update);
        }
        if !shrink_item(&mut update, ITEM_LIMIT_BYTES) {
            self.omitted += 1;
            return;
        }
        let size = json_len(&update);
        if self.entries.len() >= MAX_RECORDED_UPDATES || self.bytes + size > HARD_RECORDED_BYTES {
            self.omitted += 1;
            return;
        }
        self.bytes += size;
        self.entries.push(Entry::Update(update));
    }

    fn record_chunk(&mut self, kind: &str, update: &Value, text: &str) {
        let message_id = update
            .get("messageId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(Entry::Chunk(run)) = self.entries.last_mut() {
            if run.kind == kind && run.message_id == message_id {
                let before = run.text.retained();
                run.text.push(text, &self.secrets);
                self.bytes = (self.bytes + run.text.retained()).saturating_sub(before);
                return;
            }
        }
        let mut template = update.clone();
        if let Some(content) = template.get_mut("content").and_then(Value::as_object_mut) {
            content.insert("text".into(), Value::String(String::new()));
        }
        self.secrets.redact(&mut template);
        elide_large_leaves(&mut template);
        if json_len(&template) > ITEM_LIMIT_BYTES {
            // Huge `_meta` or annotations: keep what a reader shows.
            template = chunk_skeleton(&template);
        }
        let mut run = ChunkRun {
            kind: kind.to_owned(),
            message_id,
            update: template,
            text: RollingText::default(),
        };
        run.text.push(text, &self.secrets);
        let size = json_len(&run.update) + run.text.retained();
        if self.entries.len() >= MAX_RECORDED_UPDATES || self.bytes + size > HARD_RECORDED_BYTES {
            self.omitted += 1;
            return;
        }
        self.bytes += size;
        self.entries.push(Entry::Chunk(run));
    }

    /// Close the recorder and take the turn, or `None` when no prompt reached
    /// the agent (the turn was refused or requeued before any model work).
    pub(crate) fn finish(&mut self) -> Option<FinishedTurn> {
        if std::mem::replace(&mut self.closed, true) || !self.prompted {
            return None;
        }
        let updates = std::mem::take(&mut self.entries)
            .into_iter()
            .map(|entry| match entry {
                Entry::Update(update) => update,
                Entry::Chunk(run) => {
                    let mut update = run.update;
                    let text = run.text.finish();
                    if let Some(content) = update.get_mut("content").and_then(Value::as_object_mut)
                    {
                        content.insert("text".into(), Value::String(text));
                    }
                    update
                }
            })
            .collect();
        Some(FinishedTurn {
            updates,
            omitted: self.omitted,
            stop_reason: self.stop_reason.take().unwrap_or_else(|| "error".into()),
            usage: self.usage.take(),
            context: self.context.take(),
        })
    }
}

/// Keep only the numeric token counters a transcript carries.
fn parse_usage(usage: &Value) -> Option<TranscriptUsage> {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    let usage = TranscriptUsage {
        input_tokens: field("inputTokens"),
        output_tokens: field("outputTokens"),
        total_tokens: field("totalTokens"),
        cached_read_tokens: field("cachedReadTokens"),
        cached_write_tokens: field("cachedWriteTokens"),
    };
    (!usage.is_empty()).then_some(usage)
}

/// A closed turn, ready to split into parts.
pub(crate) struct FinishedTurn {
    pub(crate) updates: Vec<Value>,
    pub(crate) omitted: u64,
    pub(crate) stop_reason: String,
    pub(crate) usage: Option<TranscriptUsage>,
    pub(crate) context: Option<ContextWindow>,
}

/// Greedy part count for updates of `sizes` bytes under `budget` bytes of
/// update array per part, or `None` when one update alone exceeds it.
fn parts_needed(sizes: &[usize], budget: usize) -> Option<usize> {
    let mut parts = 1;
    let mut used = 0;
    for &size in sizes {
        if size > budget {
            return None;
        }
        let added = if used == 0 { size } else { size + 1 };
        if used + added > budget {
            parts += 1;
            used = size;
        } else {
            used += added;
        }
    }
    Some(parts)
}

/// Split `updates` into at most [`MAX_PARTS`] payloads of at most
/// [`MAX_PART_PLAINTEXT_BYTES`], each carrying `header`'s fields.
///
/// First, an update too large for any part is shrunk on its own
/// ([`shrink_item`]), or dropped and counted if even that fails, so one item
/// never evicts the others. Then, over budget, the oldest tool outputs
/// (`content`, `rawOutput`, `_meta`) are elided first, then the oldest
/// tool inputs (`rawInput` and every other non-skeleton field). Tool skeleton
/// fields (`toolCallId`, `title`, `kind`, `status`, `locations`) are never
/// elided. Only if that is still not enough are the oldest updates dropped,
/// chunks and plans before tool updates, and counted in `omitted`.
pub(crate) fn split_into_parts(
    header: &TranscriptPayload,
    mut updates: Vec<Value>,
    omitted: u64,
) -> Vec<TranscriptPayload> {
    let mut sizing = header.clone();
    sizing.updates = Vec::new();
    // The widest `omitted` value, so the final count always fits.
    sizing.omitted = u64::MAX;
    // Serialization is compact, so a part is exactly the header (including
    // its `"updates":[]`) plus the updates and the commas between them.
    let header_len = serde_json::to_string(&sizing).map_or(MAX_PART_PLAINTEXT_BYTES, |s| s.len());
    let budget = MAX_PART_PLAINTEXT_BYTES.saturating_sub(header_len);
    let mut sizes: Vec<usize> = updates.iter().map(json_len).collect();
    let mut omitted = omitted;
    let fits = |sizes: &[usize]| parts_needed(sizes, budget).is_some_and(|n| n <= MAX_PARTS);

    // An update too large for any part is shrunk alone, or dropped and
    // counted, before any pass can evict other updates on its account.
    let mut index = 0;
    while index < updates.len() {
        if sizes[index] > budget {
            if !shrink_item(&mut updates[index], budget) {
                updates.remove(index);
                sizes.remove(index);
                omitted += 1;
                continue;
            }
            sizes[index] = json_len(&updates[index]);
        }
        index += 1;
    }
    for outputs in [true, false] {
        for index in 0..updates.len() {
            if fits(&sizes) {
                break;
            }
            if is_tool_update(&updates[index]) && elide_tool_fields(&mut updates[index], outputs) {
                sizes[index] = json_len(&updates[index]);
            }
        }
    }
    // Last resort: drop the oldest updates, non-tool ones first.
    for drop_tools in [false, true] {
        let mut index = 0;
        while index < updates.len() && !fits(&sizes) {
            if is_tool_update(&updates[index]) == drop_tools {
                updates.remove(index);
                sizes.remove(index);
                omitted += 1;
            } else {
                index += 1;
            }
        }
    }

    let mut parts = Vec::new();
    let mut current = Vec::new();
    let mut used = 0;
    for (update, size) in updates.into_iter().zip(sizes) {
        let added = if current.is_empty() { size } else { size + 1 };
        if !current.is_empty() && used + added > budget {
            parts.push(std::mem::take(&mut current));
            used = size;
        } else {
            used += added;
        }
        current.push(update);
    }
    parts.push(current);
    parts
        .into_iter()
        .take(MAX_PARTS)
        .map(|updates| TranscriptPayload {
            omitted,
            updates,
            ..header.clone()
        })
        .collect()
}

/// Where and as whom a turn's transcript is published.
#[derive(Clone)]
pub(crate) struct Sink {
    pub(crate) keys: nostr::Keys,
    pub(crate) owner: PublicKey,
    pub(crate) gateways: Option<Arc<GatewayAccess>>,
    pub(crate) rest: RestClient,
    pub(crate) cwd: PathBuf,
    /// Secret values redacted from every recorded update.
    pub(crate) secrets: Arc<Secrets>,
    /// Publishes in flight, which harness shutdown waits on within its grace.
    pub(crate) tasks: TaskTracker,
}

/// The cleartext and header identity of one turn.
#[derive(Clone)]
struct TurnMeta {
    turn_id: String,
    turn_seq: u64,
    thread_root: EventId,
    channel: uuid::Uuid,
    started_at: String,
}

/// Records one turn and publishes its transcript when dropped, on every exit
/// path of the turn task (success, error, cancellation, or panic).
pub(crate) struct TurnTranscript {
    recorder: SharedRecorder,
    sink: Option<Sink>,
    meta: TurnMeta,
}

impl TurnTranscript {
    /// Start recording a thread turn into `acp`. Returns `None` (records
    /// nothing) for turns without a thread root or channel, or without an
    /// owner to encrypt to.
    pub(crate) fn begin(
        acp: &mut crate::acp::AcpClient,
        sink: Option<Sink>,
        channel: Option<uuid::Uuid>,
        thread_root: Option<&str>,
        turn_id: &str,
        started_at: &str,
    ) -> Option<Self> {
        let sink = sink?;
        let channel = channel?;
        let thread_root = EventId::from_hex(thread_root?).ok()?;
        let recorder = Arc::new(Mutex::new(TurnRecorder::new(Arc::clone(&sink.secrets))));
        acp.set_transcript(Some(Arc::clone(&recorder)));
        Some(Self {
            recorder,
            sink: Some(sink),
            meta: TurnMeta {
                turn_id: turn_id.to_owned(),
                turn_seq: TURN_SEQ.fetch_add(1, Ordering::Relaxed) + 1,
                thread_root,
                channel,
                started_at: started_at.to_owned(),
            },
        })
    }
}

impl Drop for TurnTranscript {
    fn drop(&mut self) {
        let finished = self
            .recorder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .finish();
        let (Some(finished), Some(sink)) = (finished, self.sink.take()) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(target: "pool::transcript", turn_id = %self.meta.turn_id,
                "NIP-AT: no runtime at turn end; transcript not published");
            return;
        };
        let meta = self.meta.clone();
        // Tracked, so harness shutdown waits for it within its grace period.
        let tasks = sink.tasks.clone();
        tasks.spawn_on(
            async move {
                let turn_id = meta.turn_id.clone();
                if tokio::time::timeout(PUBLISH_TIMEOUT, publish(sink, meta, finished))
                    .await
                    .is_err()
                {
                    tracing::warn!(target: "pool::transcript", %turn_id,
                        "NIP-AT: transcript publish timed out");
                }
            },
            &runtime,
        );
    }
}

async fn publish(sink: Sink, meta: TurnMeta, finished: FinishedTurn) {
    let header = TranscriptPayload {
        v: PAYLOAD_VERSION,
        turn_id: meta.turn_id.clone(),
        turn_seq: meta.turn_seq,
        thread_root: meta.thread_root.to_hex(),
        started_at: meta.started_at.clone(),
        ended_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        stop_reason: finished.stop_reason,
        usage: finished.usage,
        context: finished.context,
        workspace: detect_workspace(&sink.cwd).await,
        omitted: 0,
        updates: Vec::new(),
    };
    let parts = split_into_parts(&header, finished.updates, finished.omitted);
    let mut recipients = vec![sink.owner];
    if let Some(gateways) = &sink.gateways {
        match gateways.live().await {
            Ok(live) => recipients.extend(live),
            Err(error) => tracing::warn!(target: "pool::transcript", turn_id = %meta.turn_id,
                "NIP-AT: transcript to gateways skipped: {error}"),
        }
    }
    deliver(&sink, &meta, &parts, &recipients).await;
}

/// Publish `parts` to every recipient at once, so a slow submission for one
/// recipient cannot starve the others. Each recipient's parts go in order.
async fn deliver(
    sink: &Sink,
    meta: &TurnMeta,
    parts: &[TranscriptPayload],
    recipients: &[PublicKey],
) {
    futures_util::future::join_all(
        recipients
            .iter()
            .map(|recipient| deliver_to(sink, meta, parts, *recipient)),
    )
    .await;
}

async fn deliver_to(
    sink: &Sink,
    meta: &TurnMeta,
    parts: &[TranscriptPayload],
    recipient: PublicKey,
) {
    for (index, part) in parts.iter().enumerate() {
        let envelope = TranscriptEnvelope {
            recipient,
            thread_root: meta.thread_root,
            channel: meta.channel,
            turn_id: meta.turn_id.clone(),
            part_index: index,
            part_count: parts.len(),
        };
        let result = match sign_part(sink, &envelope, part) {
            Ok(event) => submit_with_retry(&sink.rest, &event).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            tracing::warn!(target: "pool::transcript", turn_id = %meta.turn_id,
                part = index, %recipient, "NIP-AT: transcript part not published: {error}");
        }
    }
}

fn sign_part(
    sink: &Sink,
    envelope: &TranscriptEnvelope,
    part: &TranscriptPayload,
) -> Result<nostr::Event, String> {
    let content = encrypt_part(&sink.keys, &envelope.recipient, part).map_err(|e| e.to_string())?;
    build_agent_session_transcript(envelope, &content)
        .map_err(|e| e.to_string())?
        .sign_with_keys(&sink.keys)
        .map_err(|e| e.to_string())
}

/// Submit one signed part, retrying with doubling backoff up to
/// [`SUBMIT_ATTEMPTS`] times. Resubmitting the same signed event is
/// idempotent: the relay stores an event id once.
async fn submit_with_retry(rest: &RestClient, event: &nostr::Event) -> Result<(), String> {
    let mut delay = SUBMIT_RETRY_BASE;
    let mut attempt = 1;
    loop {
        let error = match tokio::time::timeout(SUBMIT_TIMEOUT, rest.submit_event(event)).await {
            Ok(Ok(reply)) if reply.get("accepted") != Some(&Value::Bool(false)) => return Ok(()),
            Ok(Ok(reply)) => format!("rejected: {}", reply["message"]),
            Ok(Err(error)) => error.to_string(),
            Err(_) => "timed out".to_owned(),
        };
        if attempt >= SUBMIT_ATTEMPTS {
            return Err(format!("{error} (after {attempt} attempts)"));
        }
        tracing::debug!(target: "pool::transcript", attempt,
            "NIP-AT: transcript part submission failed, retrying: {error}");
        tokio::time::sleep(delay).await;
        delay *= 2;
        attempt += 1;
    }
}

/// The GitHub checkouts at `cwd` and directly under it (at most
/// [`MAX_WORKSPACE_ENTRIES`]), read from local refs within
/// [`WORKSPACE_TIMEOUT`]. Anything unreadable is skipped.
pub(crate) async fn detect_workspace(cwd: &Path) -> Vec<WorkspaceEntry> {
    match tokio::time::timeout(WORKSPACE_TIMEOUT, scan_workspace(cwd)).await {
        Ok(entries) => entries,
        Err(_) => {
            tracing::warn!(target: "pool::transcript", "NIP-AT: workspace scan timed out");
            Vec::new()
        }
    }
}

async fn scan_workspace(cwd: &Path) -> Vec<WorkspaceEntry> {
    let Ok(root) = tokio::fs::canonicalize(cwd).await else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    if tokio::fs::symlink_metadata(root.join(".git")).await.is_ok() {
        candidates.push(root.clone());
    }
    let mut children = Vec::new();
    if let Ok(mut dir) = tokio::fs::read_dir(&root).await {
        for _ in 0..MAX_SCANNED_ENTRIES {
            let Ok(Some(entry)) = dir.next_entry().await else {
                break;
            };
            let is_dir = entry.file_type().await.is_ok_and(|kind| kind.is_dir());
            if is_dir
                && tokio::fs::symlink_metadata(entry.path().join(".git"))
                    .await
                    .is_ok()
            {
                children.push(entry.path());
            }
        }
    }
    children.sort();
    candidates.extend(children);
    candidates.truncate(MAX_WORKSPACE_ENTRIES);
    futures_util::future::join_all(candidates.iter().map(|dir| inspect_checkout(dir)))
        .await
        .into_iter()
        .flatten()
        .collect()
}

async fn inspect_checkout(dir: &Path) -> Option<WorkspaceEntry> {
    let path = dir.to_str()?.to_owned();
    let repo = github_repo(&git(dir, &["remote", "get-url", "origin"]).await?)?;
    let head = git(dir, &["rev-parse", "--verify", "HEAD"]).await?;
    if !(head.len() == 40 || head.len() == 64)
        || !head.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let branch = git(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .filter(|branch| {
            !branch.is_empty() && branch.len() <= 255 && !branch.chars().any(char::is_control)
        });
    let pushed = match &branch {
        Some(branch) => {
            let remote = format!("refs/remotes/origin/{branch}");
            git_succeeds(dir, &["merge-base", "--is-ancestor", "HEAD", &remote]).await
        }
        None => false,
    };
    Some(WorkspaceEntry {
        path,
        repo,
        branch,
        head,
        pushed,
    })
}

fn git_command(dir: &Path, args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

/// One local git command's trimmed stdout, or `None` on failure or output
/// over [`MAX_GIT_OUTPUT_BYTES`].
async fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = git_command(dir, args).output().await.ok()?;
    if !output.status.success() || output.stdout.len() > MAX_GIT_OUTPUT_BYTES {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

async fn git_succeeds(dir: &Path, args: &[&str]) -> bool {
    git_command(dir, args)
        .stdout(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// `owner/repo` for a GitHub remote URL (HTTPS, SSH, or scp-like), or `None`
/// for any other host. Credentials in the URL never leave this function.
pub(crate) fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let path = match url.strip_prefix("git@github.com:") {
        Some(path) => path,
        None => {
            let (_, rest) = url.split_once("://")?;
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit('@').next()?;
            let host = host.split(':').next()?;
            if !host.eq_ignore_ascii_case("github.com") {
                return None;
            }
            path
        }
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    valid_github_repo(path).then(|| path.to_owned())
}

#[cfg(test)]
mod tests;
