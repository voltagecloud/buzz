//! NIP-AT turn transcripts (kind 44201) for `session/load`.
//!
//! The thread's kind 9 messages hold only the agent's text. Each agent turn
//! may also have an encrypted transcript with its tool calls, diffs, plans,
//! and usage, one copy per recipient. The bridge reads the copies addressed
//! to its signer, keeps only complete turns, and replays each one in place of
//! the agent's text for that turn. Turns without a usable transcript (older
//! turns, turns written for another gateway, or parts that are missing or do
//! not open) replay as text, as before.

use super::*;
use crate::commands::acp_bridge::frames;
use buzz_sdk::transcript::{open_agent_session_transcript, TranscriptPayload};

/// Most transcript parts read for one thread (64 turns of 8 parts).
const MAX_PARTS: usize = 512;
/// Most ciphertext bytes read for one thread.
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Time allowed to read a thread's transcripts. When it passes, the load
/// replays the thread text instead.
const TRANSCRIPT_TIMEOUT: Duration = Duration::from_secs(10);

/// One complete agent turn, with its parts' updates joined in order.
pub(in crate::commands::acp_bridge::bridge) struct ReplayTurn {
    /// `started_at`, in whole seconds (rounded down).
    pub started: u64,
    /// `ended_at`, in whole seconds (rounded up).
    pub ended: u64,
    /// `started_at` in milliseconds, which orders turns with `turn_seq`.
    started_ms: i64,
    pub payload: TranscriptPayload,
}

impl ReplayTurn {
    /// Whether a thread message created at `created_at` was posted during
    /// this turn.
    pub fn covers(&self, created_at: u64) -> bool {
        (self.started..=self.ended).contains(&created_at)
    }

    /// The `usage_update` that ends this turn's replay: the context window
    /// from the turn's latest report, with the turn's token usage as ACP
    /// `Usage` in `_meta.usage`. `None` without a valid context window.
    pub fn usage_update(&self) -> Option<Value> {
        let context = self.payload.context?;
        let mut update =
            frames::usage_update(&json!({"size": context.size, "used": context.used}))?;
        let usage = self
            .payload
            .usage
            .as_ref()
            .and_then(|usage| serde_json::to_value(usage).ok())
            .and_then(|usage| frames::prompt_usage(&usage));
        if let Some(usage) = usage {
            update["_meta"] = json!({"usage": usage});
        }
        Some(update)
    }
}

/// The complete transcript turns of the thread at `root`, oldest first. A
/// failed, slow, or oversized read is logged and gives the turns read so far
/// (or none), so the load falls back to thread text for the rest.
pub(super) async fn load_turns<R: BridgeRelay>(
    relay: &R,
    scope: &Scope,
    root: EventId,
) -> Vec<ReplayTurn> {
    match tokio::time::timeout(TRANSCRIPT_TIMEOUT, fetch_parts(relay, scope, root)).await {
        Ok(Ok(parts)) => assemble(scope, root, parts),
        Ok(Err(error)) => {
            eprintln!(
                "acp-bridge: could not read turn transcripts ({error}); replaying thread text"
            );
            Vec::new()
        }
        Err(_) => {
            eprintln!(
                "acp-bridge: turn transcripts took over {}s; replaying thread text",
                TRANSCRIPT_TIMEOUT.as_secs()
            );
            Vec::new()
        }
    }
}

/// The agent's transcript parts addressed to the signer in this thread,
/// newest first, up to [`MAX_PARTS`] and [`MAX_BYTES`]. Turns cut by a bound
/// are incomplete and replay as text.
async fn fetch_parts<R: BridgeRelay>(
    relay: &R,
    scope: &Scope,
    root: EventId,
) -> Result<Vec<Event>, CliError> {
    let mut filter = json!({
        "kinds": [buzz_core::kind::KIND_AGENT_SESSION_TRANSCRIPT],
        "authors": [scope.agent.to_hex()],
        "#p": [scope.signer.public_key().to_hex()],
        "#e": [root.to_hex()],
        "#h": [scope.channel.to_string()],
        "limit": QUERY_PAGE,
        "consistency": "strong",
    });
    let mut parts = Vec::new();
    let mut bytes = 0usize;
    let mut previous = None;
    for _ in 0..=MAX_PARTS / QUERY_PAGE {
        let mut page = relay.query(filter.clone()).await?;
        if page.len() > QUERY_PAGE {
            return Err(problem("relay exceeded transcript page bound"));
        }
        let full = page.len() == QUERY_PAGE;
        newest_first(&mut page);
        if let Some(last) = page.last() {
            let next = cursor(last);
            if previous.as_ref() == Some(&next) {
                return Err(problem("relay did not advance transcript cursor"));
            }
            apply_cursor(&mut filter, &next)?;
            previous = Some(next);
        }
        for event in page {
            bytes = bytes.saturating_add(event.content.len());
            if parts.len() >= MAX_PARTS || bytes > MAX_BYTES {
                eprintln!(
                    "acp-bridge: thread transcripts exceed the replay bound; older turns replay as text"
                );
                return Ok(parts);
            }
            parts.push(event);
        }
        if !full {
            return Ok(parts);
        }
    }
    Ok(parts)
}

struct Parts {
    count: usize,
    slots: Vec<Option<TranscriptPayload>>,
    conflict: bool,
}

/// Open each part, group the parts by (author, turn id), and keep the turns
/// whose parts are all present and agree.
fn assemble(scope: &Scope, root: EventId, events: Vec<Event>) -> Vec<ReplayTurn> {
    let mut turns: HashMap<(PublicKey, String), Parts> = HashMap::new();
    for event in events {
        let (envelope, payload) =
            match open_agent_session_transcript(&scope.signer, &scope.agent, &event) {
                Ok(opened) => opened,
                Err(error) => {
                    eprintln!("acp-bridge: ignored transcript part {}: {error}", event.id);
                    continue;
                }
            };
        if envelope.thread_root != root || envelope.channel != scope.channel {
            eprintln!(
                "acp-bridge: ignored transcript part {} for another thread",
                event.id
            );
            continue;
        }
        let parts = turns
            .entry((event.pubkey, envelope.turn_id))
            .or_insert_with(|| Parts {
                count: envelope.part_count,
                slots: vec![None; envelope.part_count],
                conflict: false,
            });
        match parts.slots.get_mut(envelope.part_index) {
            Some(slot) if parts.count == envelope.part_count => {
                if slot.as_ref().is_some_and(|existing| existing != &payload) {
                    parts.conflict = true;
                } else {
                    *slot = Some(payload);
                }
            }
            _ => parts.conflict = true,
        }
    }
    let mut complete = Vec::new();
    for ((_, turn_id), parts) in turns {
        match join(parts) {
            Some(turn) => complete.push(turn),
            None => eprintln!(
                "acp-bridge: transcript of turn {turn_id} is incomplete; replaying its thread text"
            ),
        }
    }
    complete.sort_by_key(|turn| (turn.started_ms, turn.payload.turn_seq));
    complete
}

/// Join a turn's parts in order. `None` when a part is missing, the parts
/// disagree, or the turn's times are unusable.
fn join(parts: Parts) -> Option<ReplayTurn> {
    if parts.conflict {
        return None;
    }
    let mut slots = parts.slots.into_iter();
    let mut payload = slots.next()??;
    for part in slots {
        let part = part?;
        let mut header = part.clone();
        header.updates = payload.updates.clone();
        if header != payload {
            return None;
        }
        payload.updates.extend(part.updates);
    }
    let started = chrono::DateTime::parse_from_rfc3339(&payload.started_at).ok()?;
    let ended = chrono::DateTime::parse_from_rfc3339(&payload.ended_at).ok()?;
    if ended < started {
        return None;
    }
    let started_ms = started.timestamp_millis();
    let ended = u64::try_from(ended.timestamp_millis()).ok()?.div_ceil(1000);
    let started = u64::try_from(started.timestamp()).ok()?;
    (ended >= started).then_some(ReplayTurn {
        started,
        ended,
        started_ms,
        payload,
    })
}
