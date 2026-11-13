//! NIP-AT agent session transcripts (kind 44201).
//!
//! At every turn end an agent harness publishes the turn's ACP
//! `session/update` items as one or more encrypted parts per recipient. Each
//! part is a kind 44201 event whose content is NIP-44 v2 ciphertext for one
//! recipient (the agent's owner or a live owner-delegated gateway). The
//! cleartext tags carry only the recipient, the thread root, the channel, and
//! the turn's part position.
//!
//! This module holds the wire contract: the envelope builder and parser that
//! the relay uses, the v1 payload types, and the decrypt-and-validate helper
//! for consumers. See `docs/nips/NIP-AT.md`.

use buzz_core::kind::KIND_AGENT_SESSION_TRANSCRIPT;
use buzz_core::observer::content_looks_like_nip44;
use nostr::nips::nip44;
use nostr::{Event, EventBuilder, EventId, Keys, Kind, PublicKey, Tag};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::SdkError;

/// Tag that names the turn and this event's position among its parts:
/// `["turn", <turn_id>, <part index>, <part count>]`.
pub const TURN_TAG: &str = "turn";
/// The only payload version this module writes and accepts.
pub const PAYLOAD_VERSION: u64 = 1;
/// Largest plaintext JSON of one part, in bytes.
pub const MAX_PART_PLAINTEXT_BYTES: usize = 60 * 1024;
/// Most parts one turn may have for one recipient.
pub const MAX_PARTS: usize = 8;
/// Longest accepted `turn_id`, in bytes.
pub const MAX_TURN_ID_BYTES: usize = 128;
/// Most workspace entries one payload may carry.
pub const MAX_WORKSPACE_ENTRIES: usize = 16;
/// The ACP `session/update` kinds a transcript keeps, in no particular order.
pub const KEPT_UPDATE_KINDS: [&str; 5] = [
    "agent_message_chunk",
    "agent_thought_chunk",
    "tool_call",
    "tool_call_update",
    "plan",
];

const MAX_STOP_REASON_BYTES: usize = 32;
const MAX_PATH_BYTES: usize = 4096;
const MAX_BRANCH_BYTES: usize = 255;
const MAX_REPO_PART_BYTES: usize = 100;

/// Errors from building, opening, or validating a transcript part.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    /// The event's kind, tags, or content shape break the NIP-AT envelope.
    #[error("invalid transcript envelope: {0}")]
    Envelope(String),
    /// The event signature or id does not verify.
    #[error("transcript signature does not verify")]
    Signature,
    /// The event is not signed by the expected agent.
    #[error("transcript author is not the expected agent")]
    UnexpectedAuthor,
    /// The event's `p` tag names someone other than the reader.
    #[error("transcript is addressed to another recipient")]
    NotRecipient,
    /// NIP-44 encryption or decryption failed.
    #[error("NIP-44 error: {0}")]
    Nip44(#[from] nip44::Error),
    /// The plaintext exceeds [`MAX_PART_PLAINTEXT_BYTES`].
    #[error("transcript part plaintext exceeds {max} bytes (got {got})")]
    PlaintextTooLarge {
        /// Maximum accepted plaintext bytes.
        max: usize,
        /// Actual plaintext byte count.
        got: usize,
    },
    /// The payload is not valid JSON for the v1 schema.
    #[error("transcript payload JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The payload parsed but breaks a v1 rule.
    #[error("invalid transcript payload: {0}")]
    Payload(String),
}

/// The cleartext envelope of one transcript part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEnvelope {
    /// The single reader: the owner or a live gateway (`p` tag).
    pub recipient: PublicKey,
    /// The conversation's thread root (`["e", id, "", "root"]`).
    pub thread_root: EventId,
    /// The channel the thread lives in (`h` tag).
    pub channel: Uuid,
    /// The harness turn id, shared by all parts of the turn.
    pub turn_id: String,
    /// Zero-based index of this part.
    pub part_index: usize,
    /// Number of parts the turn has for this recipient (1 to [`MAX_PARTS`]).
    pub part_count: usize,
}

/// Token usage the agent reported for the turn's final prompt. Every field is
/// a non-negative integer and is omitted when the agent did not report it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptUsage {
    /// Input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Provider-reported total tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Cache-read tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_read_tokens: Option<u64>,
    /// Cache-write tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_write_tokens: Option<u64>,
}

impl TranscriptUsage {
    /// Whether no counter is known.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The model context window from the turn's latest ACP `usage_update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextWindow {
    /// Context window size, in tokens.
    pub size: u64,
    /// Tokens in use.
    pub used: u64,
}

/// One GitHub checkout the agent worked in, read at turn end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    /// Absolute path of the checkout on the agent's host.
    pub path: String,
    /// `owner/repo` of the GitHub `origin` remote.
    pub repo: String,
    /// The checked-out branch; omitted for a detached `HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The `HEAD` commit, as lowercase hex.
    pub head: String,
    /// Whether the local `origin/<branch>` ref contains `head`.
    pub pushed: bool,
}

/// The v1 plaintext of one transcript part.
///
/// Every part of a turn carries the same header fields; `updates` holds this
/// part's slice, and a reader concatenates the slices in part order.
/// Readers ignore unknown fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptPayload {
    /// Payload version, [`PAYLOAD_VERSION`].
    pub v: u64,
    /// The harness turn id; equals the `turn` tag's id.
    pub turn_id: String,
    /// Turn sequence number, increasing within one harness process.
    pub turn_seq: u64,
    /// Thread root event id as lowercase hex; equals the `e` tag.
    pub thread_root: String,
    /// RFC 3339 time the turn began.
    pub started_at: String,
    /// RFC 3339 time the turn ended.
    pub ended_at: String,
    /// ACP stop reason (`end_turn`, `cancelled`, ...), or `error` when the
    /// agent never answered the prompt.
    pub stop_reason: String,
    /// Token usage of the turn's final prompt, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TranscriptUsage>,
    /// Latest context window report, when the agent sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextWindow>,
    /// GitHub checkouts under the harness working directory.
    #[serde(default)]
    pub workspace: Vec<WorkspaceEntry>,
    /// Updates the publisher had to drop to stay within the size limits.
    /// Omitted when zero.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: u64,
    /// ACP `session/update` `update` objects, in order.
    #[serde(default)]
    pub updates: Vec<serde_json::Value>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl TranscriptPayload {
    /// Check every v1 rule that does not depend on the envelope.
    pub fn validate(&self) -> Result<(), TranscriptError> {
        let invalid = |reason: &str| Err(TranscriptError::Payload(reason.to_owned()));
        if self.v != PAYLOAD_VERSION {
            return invalid("unsupported version");
        }
        if !valid_turn_id(&self.turn_id) {
            return invalid("turn_id is malformed");
        }
        if !is_lower_hex(&self.thread_root, 64) {
            return invalid("thread_root must be 64 lowercase hex characters");
        }
        for time in [&self.started_at, &self.ended_at] {
            if chrono::DateTime::parse_from_rfc3339(time).is_err() {
                return invalid("started_at and ended_at must be RFC 3339");
            }
        }
        if !valid_stop_reason(&self.stop_reason) {
            return invalid("stop_reason is malformed");
        }
        if self.workspace.len() > MAX_WORKSPACE_ENTRIES {
            return invalid("too many workspace entries");
        }
        if !self.workspace.iter().all(WorkspaceEntry::is_valid) {
            return invalid("malformed workspace entry");
        }
        if !self.updates.iter().all(is_kept_update) {
            return invalid("update is not a kept ACP session/update item");
        }
        Ok(())
    }
}

impl WorkspaceEntry {
    fn is_valid(&self) -> bool {
        self.path.starts_with('/')
            && self.path.len() <= MAX_PATH_BYTES
            && valid_github_repo(&self.repo)
            && self.branch.as_deref().is_none_or(valid_branch)
            && (is_lower_hex(&self.head, 40) || is_lower_hex(&self.head, 64))
    }
}

/// Whether `repo` is a GitHub `owner/repo` name.
pub fn valid_github_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    [owner, name].iter().all(|part| {
        !part.is_empty()
            && part.len() <= MAX_REPO_PART_BYTES
            && *part != "."
            && *part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    })
}

fn valid_branch(branch: &str) -> bool {
    !branch.is_empty() && branch.len() <= MAX_BRANCH_BYTES && !branch.chars().any(char::is_control)
}

/// Whether `update` is an ACP `session/update` item a transcript keeps: an
/// object whose `sessionUpdate` is one of [`KEPT_UPDATE_KINDS`], and, for tool
/// updates, a non-empty string `toolCallId`.
pub fn is_kept_update(update: &serde_json::Value) -> bool {
    let Some(kind) = update.get("sessionUpdate").and_then(|v| v.as_str()) else {
        return false;
    };
    if !KEPT_UPDATE_KINDS.contains(&kind) {
        return false;
    }
    if kind == "tool_call" || kind == "tool_call_update" {
        return update
            .get("toolCallId")
            .and_then(|v| v.as_str())
            .is_some_and(|id| !id.is_empty());
    }
    true
}

fn valid_turn_id(turn_id: &str) -> bool {
    !turn_id.is_empty()
        && turn_id.len() <= MAX_TURN_ID_BYTES
        && turn_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn valid_stop_reason(reason: &str) -> bool {
    !reason.is_empty()
        && reason.len() <= MAX_STOP_REASON_BYTES
        && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parse a canonical decimal (no sign, no leading zero).
fn canonical_usize(value: &str) -> Option<usize> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return None;
    }
    if !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn envelope_error(reason: impl Into<String>) -> TranscriptError {
    TranscriptError::Envelope(reason.into())
}

/// Build an unsigned kind 44201 transcript part. `encrypted_content` must be
/// NIP-44 v2 ciphertext for `envelope.recipient`; sign the result with the
/// agent key.
pub fn build_agent_session_transcript(
    envelope: &TranscriptEnvelope,
    encrypted_content: &str,
) -> Result<EventBuilder, SdkError> {
    if !valid_turn_id(&envelope.turn_id) {
        return Err(SdkError::InvalidInput("turn_id is malformed".into()));
    }
    if envelope.part_count == 0
        || envelope.part_count > MAX_PARTS
        || envelope.part_index >= envelope.part_count
    {
        return Err(SdkError::InvalidInput(format!(
            "part {} of {} is outside 1..={MAX_PARTS} parts",
            envelope.part_index, envelope.part_count
        )));
    }
    if !content_looks_like_nip44(encrypted_content) {
        return Err(SdkError::InvalidInput(
            "transcript content must be NIP-44 v2 ciphertext".into(),
        ));
    }
    let parse = |parts: [&str; 4], len: usize| {
        Tag::parse(parts[..len].iter().copied()).map_err(|e| SdkError::InvalidTag(e.to_string()))
    };
    let recipient = envelope.recipient.to_hex();
    let root = envelope.thread_root.to_hex();
    let channel = envelope.channel.to_string();
    let index = envelope.part_index.to_string();
    let count = envelope.part_count.to_string();
    let tags = [
        parse(["p", &recipient, "", ""], 2)?,
        parse(["e", &root, "", "root"], 4)?,
        parse(["h", &channel, "", ""], 2)?,
        parse([TURN_TAG, &envelope.turn_id, &index, &count], 4)?,
    ];
    Ok(EventBuilder::new(
        Kind::Custom(KIND_AGENT_SESSION_TRANSCRIPT as u16),
        encrypted_content,
    )
    .tags(tags))
}

/// Parse and validate the cleartext envelope of a kind 44201 event.
///
/// The event must carry exactly one each of `["p", <hex>]`,
/// `["e", <hex>, "", "root"]`, `["h", <uuid>]` and
/// `["turn", <turn_id>, <index>, <count>]`, and no other tag. The content must
/// fit the NIP-44 v2 length envelope. The relay rejects events that fail here.
pub fn parse_envelope(event: &Event) -> Result<TranscriptEnvelope, TranscriptError> {
    if event.kind != Kind::Custom(KIND_AGENT_SESSION_TRANSCRIPT as u16) {
        return Err(envelope_error("not a kind 44201 event"));
    }
    let mut recipient = None;
    let mut thread_root = None;
    let mut channel = None;
    let mut turn = None;
    for tag in event.tags.iter() {
        let parts = tag.as_slice();
        let name = parts.first().map(String::as_str).unwrap_or_default();
        let slot_taken = match name {
            "p" => {
                let [_, value] = parts else {
                    return Err(envelope_error("`p` tag must be [\"p\", <pubkey>]"));
                };
                if !is_lower_hex(value, 64) {
                    return Err(envelope_error("`p` must be 64 lowercase hex characters"));
                }
                let key = PublicKey::from_hex(value)
                    .map_err(|_| envelope_error("`p` is not a valid public key"))?;
                recipient.replace(key).is_some()
            }
            "e" => {
                let [_, id, hint, marker] = parts else {
                    return Err(envelope_error(
                        "`e` tag must be [\"e\", <id>, \"\", \"root\"]",
                    ));
                };
                if !is_lower_hex(id, 64) || !hint.is_empty() || marker != "root" {
                    return Err(envelope_error(
                        "`e` tag must be [\"e\", <id>, \"\", \"root\"]",
                    ));
                }
                let id = EventId::from_hex(id)
                    .map_err(|_| envelope_error("`e` is not a valid event id"))?;
                thread_root.replace(id).is_some()
            }
            "h" => {
                let [_, value] = parts else {
                    return Err(envelope_error("`h` tag must be [\"h\", <channel uuid>]"));
                };
                let id = value
                    .parse::<Uuid>()
                    .ok()
                    .filter(|id| id.to_string() == *value)
                    .ok_or_else(|| envelope_error("`h` must be a lowercase hyphenated UUID"))?;
                channel.replace(id).is_some()
            }
            TURN_TAG => {
                let [_, turn_id, index, count] = parts else {
                    return Err(envelope_error(
                        "`turn` tag must be [\"turn\", <id>, <index>, <count>]",
                    ));
                };
                let (Some(index), Some(count)) = (canonical_usize(index), canonical_usize(count))
                else {
                    return Err(envelope_error(
                        "`turn` part index and count must be decimal",
                    ));
                };
                if !valid_turn_id(turn_id) {
                    return Err(envelope_error("`turn` id is malformed"));
                }
                if count == 0 || count > MAX_PARTS || index >= count {
                    return Err(envelope_error(format!(
                        "`turn` part {index} of {count} is outside 1..={MAX_PARTS} parts"
                    )));
                }
                turn.replace((turn_id.clone(), index, count)).is_some()
            }
            other => {
                return Err(envelope_error(format!(
                    "unexpected `{other}` tag; only p, e, h and turn are allowed"
                )))
            }
        };
        if slot_taken {
            return Err(envelope_error(format!("more than one `{name}` tag")));
        }
    }
    let (
        Some(recipient),
        Some(thread_root),
        Some(channel),
        Some((turn_id, part_index, part_count)),
    ) = (recipient, thread_root, channel, turn)
    else {
        return Err(envelope_error(
            "exactly one each of the p, e, h and turn tags is required",
        ));
    };
    if !content_looks_like_nip44(&event.content) {
        return Err(envelope_error("content must be NIP-44 v2 ciphertext"));
    }
    Ok(TranscriptEnvelope {
        recipient,
        thread_root,
        channel,
        turn_id,
        part_index,
        part_count,
    })
}

/// Serialize `payload` and NIP-44 encrypt it from `sender` to `recipient`.
///
/// Refuses a payload that breaks a v1 rule or whose plaintext exceeds
/// [`MAX_PART_PLAINTEXT_BYTES`]. The plaintext is zeroized afterwards.
pub fn encrypt_part(
    sender: &Keys,
    recipient: &PublicKey,
    payload: &TranscriptPayload,
) -> Result<String, TranscriptError> {
    payload.validate()?;
    let mut plaintext = serde_json::to_string(payload)?;
    if plaintext.len() > MAX_PART_PLAINTEXT_BYTES {
        let got = plaintext.len();
        plaintext.zeroize();
        return Err(TranscriptError::PlaintextTooLarge {
            max: MAX_PART_PLAINTEXT_BYTES,
            got,
        });
    }
    let encrypted = nip44::encrypt(
        sender.secret_key(),
        recipient,
        &plaintext,
        nip44::Version::V2,
    );
    plaintext.zeroize();
    Ok(encrypted?)
}

/// Verify, decrypt, and validate one transcript part addressed to `reader`.
///
/// Checks, in order: the signature; that `expected_agent` signed it; the
/// envelope ([`parse_envelope`]); that the `p` tag is `reader`; that the
/// plaintext fits [`MAX_PART_PLAINTEXT_BYTES`]; the v1 payload rules; and that
/// the payload's `turn_id` and `thread_root` match the tags.
pub fn open_agent_session_transcript(
    reader: &Keys,
    expected_agent: &PublicKey,
    event: &Event,
) -> Result<(TranscriptEnvelope, TranscriptPayload), TranscriptError> {
    event.verify().map_err(|_| TranscriptError::Signature)?;
    if event.pubkey != *expected_agent {
        return Err(TranscriptError::UnexpectedAuthor);
    }
    let envelope = parse_envelope(event)?;
    if envelope.recipient != reader.public_key() {
        return Err(TranscriptError::NotRecipient);
    }
    let mut plaintext = nip44::decrypt(reader.secret_key(), &event.pubkey, &event.content)?;
    if plaintext.len() > MAX_PART_PLAINTEXT_BYTES {
        let got = plaintext.len();
        plaintext.zeroize();
        return Err(TranscriptError::PlaintextTooLarge {
            max: MAX_PART_PLAINTEXT_BYTES,
            got,
        });
    }
    let parsed = serde_json::from_str::<TranscriptPayload>(&plaintext);
    plaintext.zeroize();
    let payload = parsed?;
    payload.validate()?;
    if payload.turn_id != envelope.turn_id {
        return Err(TranscriptError::Payload(
            "turn_id differs from the turn tag".into(),
        ));
    }
    if payload.thread_root != envelope.thread_root.to_hex() {
        return Err(TranscriptError::Payload(
            "thread_root differs from the e tag".into(),
        ));
    }
    Ok((envelope, payload))
}

#[cfg(test)]
mod tests;
