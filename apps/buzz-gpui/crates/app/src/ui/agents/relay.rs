//! What the community relays say about identities: buzz-app's identity
//! archives (`src/features/relay/identity-archives.ts`). Reads use the
//! community's signed HTTP lane, so they need no live session.

use buzz_identity::Identity;
use buzz_relay_client::members::is_key;
use buzz_relay_client::{RelayHttp, RelayOrigin};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

const MAX_ARCHIVE_BYTES: usize = 2 * 1024 * 1024;

fn filter(value: Value) -> buzz_relay_client::Filter {
    match value {
        Value::Object(map) => map,
        _ => unreachable!("filters are objects"),
    }
}

/// The relay's archive snapshot: who is archived in this community.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchiveSnapshot {
    pub archived: BTreeSet<String>,
    pub head: Option<(u64, String)>,
}

/// Reads the relay-signed 13535 snapshot. `last` is the head of the
/// previous read: an older snapshot is refused rather than shown.
pub async fn read_archives(
    http: &RelayHttp,
    authority: &str,
    last: Option<&(u64, String)>,
) -> Result<ArchiveSnapshot, String> {
    let invalid = || {
        "Archive visibility is unknown. Retry to obtain a valid current relay snapshot (2 MiB limit).".to_string()
    };
    let events = http
        .query(&[filter(json!({
            "kinds": [13535], "authors": [authority], "limit": 1, "consistency": "strong",
        }))])
        .await
        .map_err(|_| invalid())?;
    let [event] = events.as_slice() else {
        if events.is_empty() {
            return Ok(ArchiveSnapshot::default());
        }
        return Err(invalid());
    };
    let markers: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("-"))
        .collect();
    let size = serde_json::to_string(event).map_or(usize::MAX, |json| json.len());
    if event.kind.as_u16() != 13535
        || event.pubkey.to_hex() != authority
        || !event.content.is_empty()
        || markers.len() != 1
        || markers[0].as_slice().len() != 1
        || size > MAX_ARCHIVE_BYTES
    {
        return Err(invalid());
    }
    let head = (event.created_at.as_secs(), event.id.to_hex());
    if let Some(last) = last
        && (head.0 < last.0 || (head.0 == last.0 && head.1 > last.1))
    {
        return Err(invalid());
    }
    let archived = event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().map(String::as_str) == Some("p"))
        .filter_map(|tag| tag.get(1))
        .filter(|key| is_key(key))
        .cloned()
        .collect();
    Ok(ArchiveSnapshot {
        archived,
        head: Some(head),
    })
}

/// A community's signed HTTP lane for the viewer.
pub fn lane(origin: &RelayOrigin, identity: Arc<Identity>) -> RelayHttp {
    RelayHttp::new(origin.clone(), identity)
}
