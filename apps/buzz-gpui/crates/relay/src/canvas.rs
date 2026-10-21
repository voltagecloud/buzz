//! The channel Canvas: one shared Markdown document per channel, stored as
//! kind-40100 revisions (`channel-templates/capability.ts` `canvas`,
//! `canvas-conflict.ts`). The newest revision (by `created_at`, then the
//! smaller id) is the document; older ones are its history.
//!
//! A save names the revision it replaces in `expected-revision` (`none`
//! for the first save), so a relay that supports Canvas compare-and-swap
//! refuses a stale save before it changes anything. The editor and the
//! Todos panel share this contract; see `Session::canvas_save`.
//!
//! This module holds the pure parts: filters, head selection, history
//! pages, the save draft and its signing shape. The session wires them to
//! the reader and the outbox (`session/canvas.rs`).

use crate::event::{Verified, is_hex64, kinds, newest_first};
use crate::messages::Draft;
use crate::outbox::{Delivery, Operation};
use crate::socket::{Filter, filter};
use nostr::prelude::{Event, EventId};
use serde_json::json;

/// Largest Canvas, in UTF-8 bytes (`model.ts` `CANVAS_BYTES`).
pub const CANVAS_BYTES: usize = 24 * 1024;
/// Revisions per history page.
pub const HISTORY_PAGE: usize = 25;

/// Why a save was refused because the Canvas moved on.
pub const CONFLICT: &str = "Canvas changed since you opened it. Your draft is kept; load the current document before replacing it.";
pub const TOO_LARGE: &str = "Keep Canvas below 24 KiB";
pub const UNAVAILABLE: &str = "Canvas is unavailable after channel access changed";
pub const WRITE_UNAVAILABLE: &str = "Canvas writing is unavailable in this channel";
pub const UNRESOLVED: &str =
    "This channel has an unresolved Canvas save. Inspect Outbox before making another save.";
pub const TOO_SOON: &str = "Please wait a second before saving Canvas again";
pub const ACCESS_CHANGED: &str = "Channel access changed";
pub const OTHER_SELECTED: &str = "Your save was received, but a different Canvas is selected. Your draft is kept; load the current document.";
pub const INVALID_HISTORY: &str = "Canvas history returned an invalid or non-advancing page";
pub const TOO_OLD: &str = "This operation could not be confirmed and is too old to replay. Keep your draft and inspect the current result before making a new save.";
pub const AWAITING: &str = "Save is awaiting exact relay confirmation; your draft is kept";

/// A failed Canvas read or save.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CanvasError {
    /// The saved Canvas is not the one the draft started from, or the
    /// relay refused the save's `expected-revision`. Nothing was written;
    /// reload before saving again (`CanvasConflictError`).
    #[error("{CONFLICT}")]
    Conflict,
    /// Anything else, as text to show. A save may still have landed.
    #[error("{0}")]
    Failed(String),
}

impl CanvasError {
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict)
    }
}

impl From<String> for CanvasError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for CanvasError {
    fn from(message: &str) -> Self {
        Self::Failed(message.to_owned())
    }
}

/// One saved Canvas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub id: EventId,
    /// Hex public key of whoever saved it.
    pub author: String,
    pub created_at: u64,
    pub content: String,
    /// The rest of the signed event (its `expected-revision` and `h`
    /// tags, kind and signature), for callers that want it whole.
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub sig: String,
}

impl Revision {
    pub fn from_event(event: &Event) -> Self {
        Self {
            id: event.id,
            author: event.pubkey.to_hex(),
            created_at: event.created_at.as_secs(),
            content: event.content.clone(),
            kind: event.kind.as_u16(),
            tags: event
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            sig: event.sig.to_string(),
        }
    }

    /// Over the save limit: it can be shown but not restored.
    pub fn too_large(&self) -> bool {
        too_large(&self.content)
    }
}

/// Where the next history page starts: older than `until`, or as old with
/// a larger id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryCursor {
    pub until: u64,
    pub before_id: EventId,
}

/// Revisions newest first; `next` when another page may follow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryPage {
    pub revisions: Vec<Revision>,
    pub next: Option<HistoryCursor>,
}

pub fn too_large(content: &str) -> bool {
    content.len() > CANVAS_BYTES
}

/// The current Canvas. Editors and save preconditions read it strongly;
/// a display-only read may use a replica.
pub fn head_filter(channel: &str, strong: bool) -> Filter {
    let mut filter = filter(json!({"kinds": [kinds::CANVAS], "#h": [channel], "limit": 1}));
    if strong {
        filter.insert("consistency".into(), json!("strong"));
    }
    filter
}

/// One history page. The first is strong so a just-saved revision shows;
/// older pages may come from a replica.
pub fn history_filter(channel: &str, cursor: Option<&HistoryCursor>) -> Filter {
    let mut filter = filter(json!({
        "kinds": [kinds::CANVAS],
        "#h": [channel],
        "limit": HISTORY_PAGE,
    }));
    match cursor {
        Some(cursor) => {
            filter.insert("until".into(), json!(cursor.until));
            filter.insert("before_id".into(), json!(cursor.before_id.to_hex()));
        }
        None => {
            filter.insert("consistency".into(), json!("strong"));
        }
    }
    filter
}

fn in_channel(event: &Event, channel: &str) -> bool {
    event.kind.as_u16() == kinds::CANVAS
        && event.tags.iter().any(|tag| {
            let tag = tag.as_slice();
            tag.first().map(String::as_str) == Some("h")
                && tag.get(1).map(String::as_str) == Some(channel)
        })
}

/// The channel's current Canvas among `events` (`selectedHead`).
pub fn selected_head(channel: &str, events: &[Verified]) -> Option<Revision> {
    events
        .iter()
        .map(|event| event.event())
        .filter(|event| in_channel(event, channel))
        .min_by(|a, b| newest_first(a, b))
        .map(Revision::from_event)
}

/// Checks and orders one history page read with `cursor`. A row from
/// another channel or kind, or one that does not advance past the cursor,
/// fails the whole page.
pub fn history_page(
    channel: &str,
    cursor: Option<&HistoryCursor>,
    rows: &[Verified],
) -> Result<HistoryPage, CanvasError> {
    let invalid = rows.iter().any(|event| {
        !in_channel(event, channel)
            || cursor.is_some_and(|cursor| {
                let at = event.created_at.as_secs();
                at > cursor.until
                    || (at == cursor.until && event.id.as_bytes() <= cursor.before_id.as_bytes())
            })
    });
    if invalid {
        return Err(INVALID_HISTORY.into());
    }
    let mut events: Vec<&Event> = rows.iter().map(|event| event.event()).collect();
    // `created_at DESC, id ASC`: the relay's order and the head rule.
    events.sort_by(|a, b| newest_first(a, b));
    events.dedup_by_key(|event| event.id);
    let revisions: Vec<Revision> = events.into_iter().map(Revision::from_event).collect();
    let next = (rows.len() >= HISTORY_PAGE)
        .then(|| revisions.last())
        .flatten()
        .map(|last| HistoryCursor {
            until: last.created_at,
            before_id: last.id,
        });
    Ok(HistoryPage { revisions, next })
}

/// The save for the outbox. `expected` is the revision the draft started
/// from; `None` claims the channel has no Canvas yet.
pub fn save_draft(
    channel: &str,
    content: &str,
    expected: Option<&EventId>,
) -> Result<Draft, CanvasError> {
    if too_large(content) {
        return Err(TOO_LARGE.into());
    }
    let tags = vec![
        vec!["h".to_owned(), channel.to_owned()],
        vec![
            "expected-revision".to_owned(),
            expected.map_or_else(|| "none".to_owned(), EventId::to_hex),
        ],
    ];
    if !valid_tags(&tags) {
        return Err(WRITE_UNAVAILABLE.into());
    }
    Ok(Draft {
        kind: kinds::CANVAS,
        content: content.to_owned(),
        tags,
    })
}

fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok()
        && value.len() == 36
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

/// The signing shape the relay broker admits for a Canvas save
/// (`canvas-signing-contract.json`): exactly one channel, at most one
/// lowercase `expected-revision`, an optional `client-id`, nothing else.
pub fn valid_tags(tags: &[Vec<String>]) -> bool {
    let count = |name: &str| {
        tags.iter()
            .filter(|tag| tag.first().map(String::as_str) == Some(name))
            .count()
    };
    count("h") == 1
        && count("expected-revision") <= 1
        && tags.iter().all(|tag| match tag.as_slice() {
            [name, value] => match name.as_str() {
                "h" => uuid(value),
                "client-id" => value.encode_utf16().count() <= 128,
                "expected-revision" => value == "none" || is_hex64(value),
                _ => false,
            },
            _ => false,
        })
}

/// A Canvas save the relay provably refused for its precondition
/// (`isDefinitiveCanvasConflict`). An uncertain outcome never counts: an
/// earlier attempt may have landed.
pub fn is_definitive_conflict(operation: &Operation) -> bool {
    operation.kind() == kinds::CANVAS
        && operation.delivery == Delivery::Failed
        && operation
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("conflict:"))
}

/// A Canvas save for `channel` whose outcome is not settled yet.
pub fn is_unresolved(operation: &Operation, channel: &str) -> bool {
    operation.kind() == kinds::CANVAS
        && operation.event.tags.iter().any(|tag| {
            let tag = tag.as_slice();
            tag.first().map(String::as_str) == Some("h")
                && tag.get(1).map(String::as_str) == Some(channel)
        })
        && !matches!(operation.delivery, Delivery::Accepted | Delivery::Seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{CHANNEL, Signer, T0};
    use serde_json::Value;

    const OTHER: &str = "00000000-0000-4000-8000-000000000002";

    fn canvas(signer: &Signer, at: u64, content: &str, channel: &str) -> Verified {
        signer.sign(
            &signer.viewer,
            kinds::CANVAS,
            at,
            content,
            &[&["h", channel]],
        )
    }

    #[test]
    fn filters_read_strongly_where_needed() {
        let strong = head_filter(CHANNEL, true);
        assert_eq!(
            Value::Object(strong),
            json!({"kinds": [40100], "#h": [CHANNEL], "limit": 1, "consistency": "strong"})
        );
        assert!(!head_filter(CHANNEL, false).contains_key("consistency"));
        let first = history_filter(CHANNEL, None);
        assert_eq!(first["limit"], json!(25));
        assert_eq!(first["consistency"], json!("strong"));
        let cursor = HistoryCursor {
            until: 7,
            before_id: EventId::from_byte_array([0xab; 32]),
        };
        let older = history_filter(CHANNEL, Some(&cursor));
        assert_eq!(older["until"], json!(7));
        assert_eq!(older["before_id"], json!("ab".repeat(32)));
        assert!(!older.contains_key("consistency"));
    }

    #[test]
    fn head_is_newest_then_smallest_id_in_the_channel() {
        let signer = Signer::new();
        let old = canvas(&signer, T0, "old", CHANNEL);
        let a = canvas(&signer, T0 + 5, "a", CHANNEL);
        let b = canvas(&signer, T0 + 5, "b", CHANNEL);
        let elsewhere = canvas(&signer, T0 + 9, "elsewhere", OTHER);
        let head = selected_head(CHANNEL, &[old, a.clone(), b.clone(), elsewhere]).unwrap();
        let smaller = if a.id.as_bytes() < b.id.as_bytes() {
            a
        } else {
            b
        };
        assert_eq!(head.id, smaller.id);
        assert_eq!(head.author, signer.viewer_hex());
        assert_eq!(selected_head(CHANNEL, &[]), None);
    }

    #[test]
    fn history_pages_order_dedupe_and_advance() {
        let signer = Signer::new();
        let rows: Vec<Verified> = (0..HISTORY_PAGE as u64)
            .map(|i| canvas(&signer, T0 + i / 2, &format!("r{i}"), CHANNEL))
            .collect();
        let page = history_page(CHANNEL, None, &rows).unwrap();
        assert_eq!(page.revisions.len(), HISTORY_PAGE);
        assert!(page.revisions.windows(2).all(|pair| {
            pair[0].created_at > pair[1].created_at
                || (pair[0].created_at == pair[1].created_at
                    && pair[0].id.as_bytes() < pair[1].id.as_bytes())
        }));
        let last = page.revisions.last().unwrap();
        assert_eq!(
            page.next,
            Some(HistoryCursor {
                until: last.created_at,
                before_id: last.id
            })
        );

        // A short page ends the history; duplicates collapse.
        let short = history_page(CHANNEL, None, &[rows[0].clone(), rows[0].clone()]).unwrap();
        assert_eq!(short.revisions.len(), 1);
        assert_eq!(short.next, None);
    }

    #[test]
    fn history_refuses_foreign_or_non_advancing_rows() {
        let signer = Signer::new();
        let here = canvas(&signer, T0, "here", CHANNEL);
        let there = canvas(&signer, T0, "there", OTHER);
        assert_eq!(
            history_page(CHANNEL, None, &[here.clone(), there]),
            Err(INVALID_HISTORY.into())
        );
        let chat = signer.sign(&signer.viewer, kinds::CHAT, T0, "hi", &[&["h", CHANNEL]]);
        assert!(history_page(CHANNEL, None, &[chat]).is_err());

        let at = |until, before_id| HistoryCursor { until, before_id };
        // Newer than the cursor, or the cursor row itself, does not advance.
        assert!(
            history_page(
                CHANNEL,
                Some(&at(T0 - 1, here.id)),
                std::slice::from_ref(&here)
            )
            .is_err()
        );
        assert!(
            history_page(CHANNEL, Some(&at(T0, here.id)), std::slice::from_ref(&here)).is_err()
        );
        let zero = EventId::from_byte_array([0; 32]);
        assert!(history_page(CHANNEL, Some(&at(T0, zero)), std::slice::from_ref(&here)).is_ok());
        assert!(history_page(CHANNEL, Some(&at(T0 + 1, here.id)), &[here]).is_ok());
    }

    #[test]
    fn save_names_its_base_and_stays_under_the_limit() {
        let first = save_draft(CHANNEL, "# Plan", None).unwrap();
        assert_eq!(first.kind, 40100);
        assert_eq!(
            first.tags,
            vec![
                vec!["h".to_owned(), CHANNEL.to_owned()],
                vec!["expected-revision".to_owned(), "none".to_owned()],
            ]
        );
        let base = EventId::from_byte_array([0xcd; 32]);
        let next = save_draft(CHANNEL, "", Some(&base)).unwrap();
        assert_eq!(next.tags[1][1], "cd".repeat(32));

        // Bytes, not characters: 24 KiB of a 3-byte character is too much.
        assert!(save_draft(CHANNEL, &"a".repeat(CANVAS_BYTES), None).is_ok());
        assert_eq!(
            save_draft(CHANNEL, &"a".repeat(CANVAS_BYTES + 1), None),
            Err(TOO_LARGE.into())
        );
        assert!(too_large(&"€".repeat(CANVAS_BYTES / 3 + 1)));
        assert_eq!(
            save_draft("invalid", "x", None),
            Err(WRITE_UNAVAILABLE.into())
        );
    }

    fn operation(delivery: Delivery, error: Option<&str>, channel: &str) -> Operation {
        let signer = Signer::new();
        let event = canvas(&signer, T0, "x", channel);
        let mut unsigned = nostr::prelude::UnsignedEvent::new(
            event.pubkey,
            event.created_at,
            event.kind,
            event.tags.clone(),
            event.content.clone(),
        );
        unsigned.id = Some(event.id);
        Operation {
            event: unsigned,
            signed: Some(event.event().clone()),
            recovery: None,
            acknowledged: false,
            guarded: false,
            delivery,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn only_a_proven_refusal_is_a_conflict() {
        let refused = operation(Delivery::Failed, Some("conflict: stale"), CHANNEL);
        assert!(is_definitive_conflict(&refused));
        assert!(!is_definitive_conflict(&operation(
            Delivery::Unknown,
            Some("conflict: stale"),
            CHANNEL
        )));
        assert!(!is_definitive_conflict(&operation(
            Delivery::Failed,
            Some("Relay rejected the message (400)"),
            CHANNEL
        )));
        assert!(!is_definitive_conflict(&operation(
            Delivery::Failed,
            None,
            CHANNEL
        )));
        assert!(CanvasError::Conflict.is_conflict());
        assert_eq!(CanvasError::Conflict.to_string(), CONFLICT);
    }

    #[test]
    fn unresolved_saves_block_only_their_channel() {
        for delivery in [Delivery::Sending, Delivery::Unknown, Delivery::Failed] {
            assert!(is_unresolved(&operation(delivery, None, CHANNEL), CHANNEL));
        }
        for delivery in [Delivery::Accepted, Delivery::Seen] {
            assert!(!is_unresolved(&operation(delivery, None, CHANNEL), CHANNEL));
        }
        assert!(!is_unresolved(
            &operation(Delivery::Unknown, None, OTHER),
            CHANNEL
        ));
    }

    /// buzz-app's shared corpus of Canvas signing shapes, checked against
    /// this module and buzz-desktop's signer admission alike.
    #[test]
    fn signing_shape_matches_the_shared_contract() {
        let corpus: Value =
            serde_json::from_str(include_str!("canvas/canvas-signing-contract.json")).unwrap();
        let cases = corpus.as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let accepted = case["accepted"].as_bool().unwrap();
            let Ok(tags) = serde_json::from_value::<Vec<Vec<String>>>(case["tags"].clone()) else {
                assert_eq!(case["deserializes"], json!(false), "{name}");
                assert!(!accepted, "{name}");
                continue;
            };
            assert_eq!(valid_tags(&tags), accepted, "{name}");
            let template = buzz_desktop::EventTemplate {
                created_at: T0,
                kind: kinds::CANVAS,
                tags,
                content: "Canvas".into(),
            };
            let admitted =
                buzz_desktop::relay::events::validate_event("https://relay.example", &template);
            assert_eq!(admitted.is_ok(), accepted, "{name}: {admitted:?}");
        }
    }
}
