//! Channel history pages (NIP-CW channel mode, spec §5b.1).
//!
//! A page is one `top_level` read. The relay signs a 39006 bound into every
//! response naming where the next older page starts; the cursor is echoed
//! back verbatim and must strictly advance.

use crate::event::{Verified, is_hex64, kinds};
use crate::socket::Filter;
use serde_json::{Value, json};

/// Rows per page, for the head and every older page.
pub const PAGE_SIZE: u64 = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub created_at: u64,
    /// 64-char hex event id, echoed as the relay sent it.
    pub id: String,
}

#[derive(Debug)]
pub struct Page {
    /// Everything but the bound: rows, their overlays, thread summaries.
    pub events: Vec<Verified>,
    pub has_more: bool,
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Relay returned an invalid history page: {0}")]
pub struct InvalidPage(&'static str);

pub fn page_filter(channel: &str, cursor: Option<&Cursor>) -> Filter {
    let mut filter = json!({
        "kinds": kinds::CHANNEL_ROW_KINDS,
        "#h": [channel],
        "limit": PAGE_SIZE,
        "top_level": true,
        "include_aux": true,
        "include_summaries": true,
    });
    if let Some(cursor) = cursor {
        filter["until"] = json!(cursor.created_at);
        filter["before_id"] = json!(cursor.id);
    }
    crate::socket::filter(filter)
}

/// Validates the page's bound (`window.ts:89-142`) and splits it off.
pub fn parse_page(
    channel: &str,
    relay_author: &str,
    sent: Option<&Cursor>,
    events: &[Verified],
) -> Result<Page, InvalidPage> {
    let expected_d = match sent {
        None => format!("{channel}:head"),
        Some(cursor) => format!("{channel}:{}:{}", cursor.created_at, cursor.id),
    };
    let mut bound = None;
    let mut rest = Vec::with_capacity(events.len());
    for event in events {
        if event.kind_u16() == kinds::CHANNEL_WINDOW {
            if event.pubkey_hex() != relay_author
                || event.tag("h") != Some(channel)
                || event.tag("d") != Some(expected_d.as_str())
            {
                return Err(InvalidPage("bound does not match the request"));
            }
            if bound.replace(event).is_some() {
                return Err(InvalidPage("more than one bound"));
            }
        } else {
            rest.push(event.clone());
        }
    }
    let bound = bound.ok_or(InvalidPage("missing bound"))?;
    let content: Value =
        serde_json::from_str(&bound.content).map_err(|_| InvalidPage("bound content"))?;
    let has_more = content
        .get("has_more")
        .and_then(Value::as_bool)
        .ok_or(InvalidPage("bound has_more"))?;
    let cursor = match content.get("next_cursor") {
        None | Some(Value::Null) => None,
        Some(Value::Object(next)) => {
            let created_at = next
                .get("created_at")
                .and_then(Value::as_u64)
                .filter(|t| *t < (1 << 53))
                .ok_or(InvalidPage("cursor time"))?;
            let id = next
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| is_hex64(id))
                .ok_or(InvalidPage("cursor id"))?;
            Some(Cursor {
                created_at,
                id: id.to_string(),
            })
        }
        Some(_) => return Err(InvalidPage("cursor shape")),
    };
    if has_more != cursor.is_some() {
        return Err(InvalidPage("has_more disagrees with the cursor"));
    }
    if let (Some(sent), Some(next)) = (sent, &cursor) {
        let advanced = next.created_at < sent.created_at
            || (next.created_at == sent.created_at && next.id > sent.id);
        if !advanced {
            return Err(InvalidPage("cursor did not advance"));
        }
    }
    Ok(Page {
        events: rest,
        has_more,
        cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Verifier;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag};

    fn bound(relay: &Keys, d: &str, channel: &str, content: Value) -> Verified {
        let event = EventBuilder::new(Kind::from_u16(39006), content.to_string())
            .tags([Tag::custom("d", [d]), Tag::custom("h", [channel])])
            .finalize(relay)
            .unwrap();
        Verifier::default().verify(event).unwrap()
    }

    fn row(author: &Keys, channel: &str) -> Verified {
        let event = EventBuilder::new(Kind::from_u16(9), "hi")
            .tags([Tag::custom("h", [channel])])
            .finalize(author)
            .unwrap();
        Verifier::default().verify(event).unwrap()
    }

    #[test]
    fn head_page_with_more_history() {
        let relay = Keys::generate();
        let author = Keys::generate();
        let next =
            json!({"has_more": true, "next_cursor": {"created_at": 100, "id": "a".repeat(64)}});
        let events = vec![row(&author, "c"), bound(&relay, "c:head", "c", next)];
        let page = parse_page("c", &relay.public_key().to_hex(), None, &events).unwrap();
        assert_eq!(page.events.len(), 1);
        assert!(page.has_more);
        assert_eq!(page.cursor.unwrap().created_at, 100);
    }

    #[test]
    fn refuses_forged_mismatched_or_stuck_bounds() {
        let relay = Keys::generate();
        let relay_hex = relay.public_key().to_hex();
        let end = json!({"has_more": false, "next_cursor": null});
        // Signed by someone else.
        let forged = bound(&Keys::generate(), "c:head", "c", end.clone());
        assert!(parse_page("c", &relay_hex, None, &[forged]).is_err());
        // Bound for a different request.
        let other = bound(&relay, "c:5:x", "c", end.clone());
        assert!(parse_page("c", &relay_hex, None, &[other]).is_err());
        // Missing entirely.
        assert!(parse_page("c", &relay_hex, None, &[]).is_err());
        // has_more without a cursor.
        let liar = bound(
            &relay,
            "c:head",
            "c",
            json!({"has_more": true, "next_cursor": null}),
        );
        assert!(parse_page("c", &relay_hex, None, &[liar]).is_err());
        // A cursor that does not move past the one sent.
        let sent = Cursor {
            created_at: 100,
            id: "b".repeat(64),
        };
        let stuck = bound(
            &relay,
            &format!("c:100:{}", "b".repeat(64)),
            "c",
            json!({"has_more": true, "next_cursor": {"created_at": 100, "id": "a".repeat(64)}}),
        );
        assert!(parse_page("c", &relay_hex, Some(&sent), &[stuck]).is_err());
    }

    #[test]
    fn older_page_filter_echoes_the_cursor() {
        let cursor = Cursor {
            created_at: 7,
            id: "f".repeat(64),
        };
        let filter = page_filter("c", Some(&cursor));
        assert_eq!(filter["until"], json!(7));
        assert_eq!(filter["before_id"], json!("f".repeat(64)));
        assert_eq!(filter["limit"], json!(PAGE_SIZE));
    }
}
