//! The messages before a request, for its context (buzz-app's
//! `history.ts`): read as the owner, folded with their edits and deletions
//! as the channel shows them, and cut off at the request itself.

use super::prompt::Conversation;
use crate::buzz_mcp::BuzzEvent;
use buzz_relay_client::fold::{FoldOptions, fold_messages};
use buzz_relay_client::{Verified, Verifier};
use futures::future::BoxFuture;
use nostr::prelude::PublicKey;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;

/// Messages shown, and characters of each.
const MESSAGES: usize = 50;
const CHARACTERS: usize = 4000;

/// Reads the community as the owner.
pub type Read<'a> =
    &'a (dyn Fn(Vec<Value>) -> BoxFuture<'static, Result<Vec<BuzzEvent>, String>> + Send + Sync);

#[derive(Serialize)]
struct Message {
    id: String,
    pubkey: String,
    created_at: u64,
    content: String,
}

#[derive(Serialize)]
struct History {
    note: &'static str,
    messages: Vec<Message>,
}

/// A key that signs nothing here: no relay-signed rows are read.
const NO_RELAY: &str = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

/// Up to 50 messages before `event` in its thread, or in the channel when
/// the session is the whole channel, as a `<thread-context>` or
/// `<conversation-context>` section.
pub async fn conversation_history(
    event: &BuzzEvent,
    conversation: &Conversation,
    read: Read<'_>,
) -> Result<String, String> {
    let channel = conversation.channel_id.as_str();
    let root = conversation.root.as_deref();
    let filter = |extra: Value| {
        let mut filter = json!({ "kinds": [9, 40002], "#h": [channel], "until": event.created_at });
        if let (Some(filter), Value::Object(extra)) = (filter.as_object_mut(), extra) {
            filter.extend(extra);
        }
        filter
    };
    let found = read(match root {
        Some(root) => vec![
            filter(json!({ "ids": [root], "limit": 1 })),
            filter(json!({ "#e": [root], "limit": MESSAGES })),
        ],
        None => vec![filter(json!({ "limit": MESSAGES }))],
    })
    .await?;
    let ids: Vec<&str> = found.iter().map(|message| message.id.as_str()).collect();
    let changes = if ids.is_empty() {
        Vec::new()
    } else {
        read(vec![json!({
            "kinds": [40003, 5, 9005],
            "#h": [channel],
            "#e": ids,
            "limit": 500,
            "until": event.created_at,
        })])
        .await?
    };
    // Relay `until` is inclusive to the second. Apply Buzz's full ordering
    // before folding so later same-second messages, edits and deletions
    // cannot leak in.
    let verifier = Verifier::new(found.len() + changes.len() + 1);
    let verify = |item: &BuzzEvent| -> Option<Verified> {
        verifier.verify_json(&serde_json::to_value(item).ok()?).ok()
    };
    let cutoff = (ms(event), event.id.clone());
    let mut seen = HashSet::new();
    let earlier: Vec<Verified> = found
        .iter()
        .chain(&changes)
        .filter(|item| seen.insert(item.id.clone()))
        .filter(|item| (ms(item), item.id.clone()) < cutoff)
        .filter_map(verify)
        .collect();
    let relay = PublicKey::from_hex(NO_RELAY).map_err(|error| error.to_string())?;
    let options = FoldOptions {
        include_replies: true,
        ..Default::default()
    };
    let rows: Vec<_> = fold_messages(channel, &relay, &earlier, &options)
        .into_iter()
        .filter(|message| {
            root.is_none_or(|root| {
                message.id.to_hex() == root
                    || message
                        .thread_root_id
                        .is_some_and(|thread| thread.to_hex() == root)
            })
        })
        .collect();
    let messages = rows[rows.len().saturating_sub(MESSAGES)..]
        .iter()
        .map(|message| Message {
            id: message.id.to_hex(),
            pubkey: message.author.to_hex(),
            created_at: message.created_at,
            content: message
                .source_content
                .as_ref()
                .unwrap_or(&message.content)
                .chars()
                .take(CHARACTERS)
                .collect(),
        })
        .collect();
    let section = if root.is_some() {
        "thread-context"
    } else {
        "conversation-context"
    };
    let text = super::prompt::quoted(&History {
        note: "Recent context, not new requests. Previously supplied context may repeat. Up to 50 messages, 4000 characters each.",
        messages,
    });
    Ok(format!("<{section}>\n{text}\n</{section}>\n"))
}

/// `created_at` in milliseconds with its `ms` tag, as the channel orders
/// messages (the relay client's `event_ms`, for events not yet parsed).
fn ms(event: &BuzzEvent) -> u64 {
    let offset = event
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some("ms"))
        .and_then(|tag| tag.get(1));
    event.created_at * 1000 + offset.map_or(0, |offset| millis(offset))
}

/// A canonical 0-999 `ms` value; anything else is 0.
fn millis(value: &str) -> u64 {
    let bytes = value.as_bytes();
    let canonical = (1..=3).contains(&bytes.len())
        && bytes.iter().all(u8::is_ascii_digit)
        && (bytes[0] != b'0' || bytes.len() == 1);
    if canonical {
        value.parse().unwrap_or(0)
    } else {
        0
    }
}

#[cfg(test)]
mod tests;
