//! Thread reads and their relay-signed bounds (`thread-window.ts`,
//! `threads.ts` filters; spec §5c.3–§5c.5).
//!
//! A strict thread page is one NIP-CW `thread_window` filter. The relay
//! answers with one 39007 per filter whose `d` tag is a SHA-256 binding of
//! the normalized request, so a bound can never be replayed for a different
//! viewer, host, root, page size or cursor.

use crate::event::{Verified, is_hex64, kinds};
use crate::socket::{Filter, filter};
use nostr::prelude::EventId;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// First strict page of a run (`INITIAL_WINDOW_SIZE`).
pub const INITIAL_WINDOW_SIZE: u64 = 10;
/// Every later page, strict or legacy.
pub const PAGE_SIZE: u64 = 50;
/// Deepest reply a thread view asks for. The relay caps depth at 100.
pub const DEPTH_LIMIT: u64 = 100;
/// The relay's `thread_window` row allowlist excludes legacy diffs (40008).
pub const STRICT_WINDOW_KINDS: [u16; 2] = [kinds::CHAT, kinds::AGENT_MESSAGE];
/// Content kinds a thread shows, and the legacy traversal's `kinds`.
pub use crate::unread::CONTENT_KINDS;
/// Overlays fetched for an exact target (`threads.ts:356-364`).
pub const TARGET_OVERLAY_KINDS: [u16; 5] = [
    kinds::DELETION,
    kinds::REACTION,
    kinds::MODERATOR_DELETE,
    kinds::EDIT,
    kinds::THREAD_SUMMARY,
];
/// Deletions of those overlays.
pub const TOMBSTONE_KINDS: [u16; 2] = [kinds::DELETION, kinds::MODERATOR_DELETE];
/// Limit of the exact overlay reads; a response this large is refused.
pub const EVIDENCE_LIMIT: usize = 500;
/// chrono 0.4's `DateTime::MAX_UTC`, the largest cursor the relay can form.
pub const MAX_CURSOR_TIME: u64 = 8_210_266_876_799;
/// Keys the relay accepts in a `thread_window` filter. Anything else, such
/// as `consistency` (§9.3 #15), is a 400.
pub const WINDOW_FIELDS: [&str; 9] = [
    "thread_window",
    "#h",
    "#e",
    "kinds",
    "limit",
    "depth_limit",
    "until",
    "before_id",
    "include_aux",
];

/// A position in a thread traversal. Strict cursors come from a 39007 and
/// are echoed verbatim; legacy cursors are the last loaded reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadCursor {
    pub created_at: u64,
    /// 64-char lowercase hex event id.
    pub id: String,
}

impl ThreadCursor {
    pub fn of(event: &Verified) -> Self {
        Self {
            created_at: event.created_at_secs(),
            id: event.id_hex(),
        }
    }

    /// Relay cursor order: seconds, then id.
    pub fn cmp_event(&self, event: &Verified) -> std::cmp::Ordering {
        self.created_at
            .cmp(&event.created_at_secs())
            .then_with(|| self.id.as_str().cmp(event.id_hex().as_str()))
    }
}

/// What one verified 39007 says about the next older page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadBounds {
    pub has_more: bool,
    pub cursor: Option<ThreadCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ThreadWindowError {
    /// The relay served no 39007 at all. `legacy_rows` is set when the
    /// response still held rows for the requested channel and root: an old
    /// relay that ignored `thread_window`. Only the first probe of a view may
    /// treat that as a reason to restart in legacy mode.
    #[error("Relay did not serve a signed thread window")]
    Missing { legacy_rows: bool },
    #[error("Thread window bounds are invalid")]
    Invalid,
}

/// `/^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$/` (`thread-window.ts:14`).
/// Only such channels can use strict windows; others go straight to legacy.
pub use crate::discovery::is_uuid as is_canonical_channel;

/// `{ids:[id], "#h":[channel], limit:1}`: the selected message or the root.
/// Strict mode reads it alone; legacy mode batches it with the page.
pub fn message_filter(channel: &str, id: &EventId) -> Filter {
    filter(json!({"ids": [id.to_hex()], "#h": [channel], "limit": 1}))
}

/// One strict `thread_window` page (§5c.3 step 3). Only the relay's
/// allowlisted keys appear; never `consistency` (§9.3 #15).
pub fn window_filter(
    channel: &str,
    root: &EventId,
    limit: u64,
    cursor: Option<&ThreadCursor>,
) -> Filter {
    let mut filter = json!({
        "kinds": STRICT_WINDOW_KINDS,
        "#h": [channel],
        "#e": [root.to_hex()],
        "depth_limit": DEPTH_LIMIT,
        "limit": limit,
        "include_aux": true,
        "thread_window": true,
    });
    if let Some(cursor) = cursor {
        filter["until"] = json!(cursor.created_at);
        filter["before_id"] = json!(cursor.id);
    }
    let filter = crate::socket::filter(filter);
    debug_assert!(
        filter
            .keys()
            .all(|key| WINDOW_FIELDS.contains(&key.as_str()))
    );
    filter
}

/// One legacy oldest-first page (§5c.3 step 4). The relay never returns
/// the root on this path, so callers batch [`message_filter`] with it.
pub fn legacy_filter(channel: &str, root: &EventId, cursor: Option<&ThreadCursor>) -> Filter {
    let mut filter = json!({
        "kinds": CONTENT_KINDS,
        "#h": [channel],
        "#e": [root.to_hex()],
        "depth_limit": DEPTH_LIMIT,
        "limit": PAGE_SIZE,
        "include_aux": true,
    });
    if let Some(cursor) = cursor {
        filter["thread_cursor"] = json!(cursor.created_at);
        filter["thread_cursor_id"] = json!(cursor.id);
    }
    crate::socket::filter(filter)
}

/// Overlays of an exact target. ID reads do not expand overlays, and the
/// overlays carry no `h` tag requirement.
pub fn overlays_filter(target: &EventId) -> Filter {
    filter(json!({
        "kinds": TARGET_OVERLAY_KINDS,
        "#e": [target.to_hex()],
        "limit": EVIDENCE_LIMIT,
    }))
}

/// Deletions of an exact target's overlays.
pub fn tombstones_filter(overlays: &[EventId]) -> Filter {
    let ids: Vec<String> = overlays.iter().map(EventId::to_hex).collect();
    filter(json!({"kinds": TOMBSTONE_KINDS, "#e": ids, "limit": EVIDENCE_LIMIT}))
}

/// The relay authority a binding names (`thread-window.ts:17-29`, the same
/// normalization as `buzz-core::tenant::normalize_host`): lowercase host,
/// non-default port kept, `:80`/`:443` dropped, one trailing `.` dropped.
/// Credentials or a scheme other than http(s)/ws(s) are refused. This must
/// be the relay's own origin, never a local proxy's.
pub fn thread_authority(origin: &str) -> Result<String, ThreadWindowError> {
    let url = url::Url::parse(origin).map_err(|_| ThreadWindowError::Invalid)?;
    if !matches!(url.scheme(), "https" | "http" | "wss" | "ws")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ThreadWindowError::Invalid);
    }
    let mut host = url
        .host_str()
        .ok_or(ThreadWindowError::Invalid)?
        .to_ascii_lowercase();
    if let Some(port) = url.port() {
        host = format!("{host}:{port}");
    }
    if let Some(stripped) = host
        .strip_suffix(":80")
        .or_else(|| host.strip_suffix(":443"))
    {
        host = stripped.to_string();
    }
    if let Some(stripped) = host.strip_suffix('.') {
        host = stripped.to_string();
    }
    Ok(host)
}

/// NIP-CW thread-mode v1 binding, byte-identical to
/// `buzz-core::thread_window::Request::binding`: `tw:1:` and the SHA-256 of
/// the compact JSON array
/// `["tw",1,"older",host,viewer,channel,root,limit,depth,kinds,cursor,aux]`.
///
/// Absent `limit`, `depth_limit` and `include_aux` take the relay's
/// defaults (50, 100, false). The reference sends `null` for a missing
/// limit, which the relay would never match; every filter built here sets
/// it, so the two agree on everything this client sends.
pub fn thread_binding(filter: &Filter, host: &str, viewer: &str) -> String {
    let first = |key: &str| {
        filter
            .get(key)
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut kinds: Vec<u64> = filter
        .get("kinds")
        .and_then(Value::as_array)
        .map(|kinds| kinds.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    kinds.sort_unstable();
    kinds.dedup();
    let cursor = match filter.get("until") {
        None => Value::Null,
        Some(until) => json!([
            until,
            filter.get("before_id").cloned().unwrap_or(Value::Null)
        ]),
    };
    let canonical = json!([
        "tw",
        1,
        "older",
        host,
        viewer,
        first("#h"),
        first("#e"),
        filter.get("limit").cloned().unwrap_or(json!(50)),
        filter
            .get("depth_limit")
            .cloned()
            .unwrap_or(json!(DEPTH_LIMIT)),
        kinds,
        cursor,
        filter.get("include_aux").cloned().unwrap_or(json!(false)),
    ]);
    let digest = Sha256::digest(canonical.to_string().as_bytes());
    let mut binding = String::with_capacity(5 + 64);
    binding.push_str("tw:1:");
    for byte in digest {
        binding.push_str(&format!("{byte:02x}"));
    }
    binding
}

/// Parses a 39007 body (`thread-window.ts:63-89`).
pub fn thread_bounds(content: &str) -> Result<ThreadBounds, ThreadWindowError> {
    let invalid = ThreadWindowError::Invalid;
    let body: Map<String, Value> = match serde_json::from_str(content) {
        Ok(Value::Object(body)) => body,
        _ => return Err(invalid),
    };
    if body.get("version").and_then(Value::as_f64) != Some(1.0)
        || body.get("direction").and_then(Value::as_str) != Some("older")
    {
        return Err(invalid);
    }
    let has_more = body
        .get("has_more")
        .and_then(Value::as_bool)
        .ok_or(ThreadWindowError::Invalid)?;
    let cursor = match body.get("next_cursor") {
        Some(Value::Null) => None,
        Some(Value::Object(next)) => {
            let created_at = next
                .get("created_at")
                .and_then(safe_integer)
                .filter(|at| *at <= MAX_CURSOR_TIME)
                .ok_or(ThreadWindowError::Invalid)?;
            let id = next
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| is_hex64(id))
                .ok_or(ThreadWindowError::Invalid)?;
            Some(ThreadCursor {
                created_at,
                id: id.to_string(),
            })
        }
        // Absent is not null: the relay always writes the field.
        _ => return Err(invalid),
    };
    if has_more != cursor.is_some() {
        return Err(invalid);
    }
    Ok(ThreadBounds { has_more, cursor })
}

/// A non-negative JS safe integer.
fn safe_integer(value: &Value) -> Option<u64> {
    const SAFE: u64 = (1 << 53) - 1;
    value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|n| n.fract() == 0.0 && *n >= 0.0 && *n <= SAFE as f64)
                .map(|n| n as u64)
        })
        .filter(|n| *n <= SAFE)
}

fn single<'a>(filter: &'a Filter, key: &str) -> Option<&'a str> {
    filter
        .get(key)
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(Value::as_str)
}

fn is_window(filter: &Filter) -> bool {
    filter.get("thread_window") == Some(&Value::Bool(true))
}

/// Checks the 39007s of a verified response against the filters that were
/// sent (`thread-window.ts:93-158`, §5c.4). Run it on every read before
/// the response reaches any store; a failure discards the whole response.
///
/// Returns `Ok(None)` when no filter is a thread window, otherwise one
/// [`ThreadBounds`] per filter in filter order. `host` is
/// [`thread_authority`] of the relay origin; `viewer` and `relay_author`
/// are lowercase hex keys.
pub fn verify_thread_windows(
    filters: &[Filter],
    events: &[Verified],
    host: Option<&str>,
    viewer: &str,
    relay_author: &str,
) -> Result<Option<Vec<ThreadBounds>>, ThreadWindowError> {
    let invalid = ThreadWindowError::Invalid;
    if !filters.iter().any(is_window) {
        return Ok(None);
    }
    // A missing authority is a configuration error, never a reason to
    // fall back to unsigned pages.
    let host = host
        .filter(|host| !host.is_empty())
        .ok_or(invalid.clone())?;
    if !is_hex64(viewer) || !is_hex64(relay_author) || !filters.iter().all(is_window) {
        return Err(invalid);
    }
    let bounds: Vec<&Verified> = events
        .iter()
        .filter(|event| event.kind_u16() == kinds::THREAD_WINDOW)
        .collect();
    if bounds.is_empty() {
        let legacy_rows = events.iter().any(|event| {
            filters.iter().any(|filter| {
                let kind = u64::from(event.kind_u16());
                filter
                    .get("kinds")
                    .and_then(Value::as_array)
                    .is_some_and(|kinds| kinds.iter().any(|k| k.as_u64() == Some(kind)))
                    && single(filter, "#h").is_some_and(|h| event.has_tag("h", h))
                    && single(filter, "#e").is_some_and(|e| event.has_tag("e", e))
            })
        });
        return Err(ThreadWindowError::Missing { legacy_rows });
    }
    if bounds.len() != filters.len() {
        return Err(invalid);
    }
    let mut remaining: Vec<&Verified> = bounds.clone();
    let mut result = Vec::with_capacity(filters.len());
    for filter in filters {
        let binding = thread_binding(filter, host, viewer);
        let channel = single(filter, "#h").ok_or(invalid.clone())?;
        let root = single(filter, "#e").ok_or(invalid.clone())?;
        let bound = bounds
            .iter()
            .find(|event| event.has_tag("d", &binding))
            .ok_or(invalid.clone())?;
        let Some(position) = remaining.iter().position(|event| event.id == bound.id) else {
            return Err(invalid);
        };
        remaining.remove(position);
        let exactly_one = |name: &str, value: &str| {
            bound
                .tags
                .iter()
                .filter(|tag| matches!(tag.as_slice(), [n, v] if n == name && v == value))
                .count()
                == 1
        };
        if bound.pubkey_hex() != relay_author
            || bound.tags.len() != 3
            || !exactly_one("d", &binding)
            || !exactly_one("h", channel)
            || !exactly_one("e", root)
        {
            return Err(invalid);
        }
        let parsed = thread_bounds(&bound.content)?;
        if let (Some(cursor), Some(until)) = (&parsed.cursor, filter.get("until")) {
            let until = until.as_u64().ok_or(invalid.clone())?;
            let before = filter
                .get("before_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let advanced = cursor.created_at < until
                || (cursor.created_at == until && cursor.id.as_str() > before);
            if !advanced {
                return Err(invalid);
            }
        }
        result.push(parsed);
    }
    Ok(Some(result))
}
