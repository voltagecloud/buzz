//! An agent's memories as its owner reads them (`features/agents/memory.ts`
//! and the native `relay_agent_memories_read`): kind 30174 events the agent
//! addressed to the viewer, decrypted and checked by the viewer's identity.

use super::Session;
use crate::event::is_hex64;
use crate::http::HttpError;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::Duration;

/// Records one read returns at most (`MEMORY_EVENT_LIMIT`).
const MEMORY_EVENTS: usize = 256;
/// The response's byte budget (`MEMORY_WIRE_BYTES`).
const MEMORY_WIRE_BYTES: usize = 2 << 20;
/// The decoded listing's byte budget (`MEMORY_TEXT_BYTES`).
const MEMORY_TEXT_BYTES: usize = 1 << 20;
const MEMORY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryEntry {
    /// `core` or `mem/...`.
    pub slug: String,
    pub body: String,
    pub event_id: String,
    pub created_at: u64,
}

/// The newest memory per slug, ordered by slug.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryListing {
    pub entries: Vec<MemoryEntry>,
    /// Some records were invalid, or the read hit its limit.
    pub partial: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    /// The relay refused the read (401 or 403).
    #[error("Memory read failed")]
    Denied,
    #[error("{0}")]
    Failed(String),
}

/// Whether `agent` can have memories for `viewer` (`memoryAgent`).
pub fn memory_agent(agent: &str, viewer: &str) -> bool {
    is_hex64(agent) && agent != viewer
}

/// Validates the decoded listing before keeping its plaintext
/// (`memoryListing`): unique valid slugs, hex ids, and a byte budget.
pub fn memory_listing(value: &Value) -> Result<MemoryListing, MemoryError> {
    let invalid = || MemoryError::Failed("Invalid memory listing".into());
    let partial = value["partial"].as_bool().ok_or_else(invalid)?;
    let entries = value["entries"].as_array().ok_or_else(invalid)?;
    if entries.len() > MEMORY_EVENTS {
        return Err(invalid());
    }
    let mut slugs = HashSet::new();
    let mut bytes = 0;
    let mut listing = MemoryListing {
        entries: Vec::with_capacity(entries.len()),
        partial,
    };
    for entry in entries {
        let bad = || MemoryError::Failed("Invalid memory entry".into());
        let slug = entry["slug"].as_str().ok_or_else(bad)?;
        let body = entry["body"].as_str().ok_or_else(bad)?;
        let event_id = entry["eventId"].as_str().ok_or_else(bad)?;
        let created_at = entry["createdAt"].as_u64().ok_or_else(bad)?;
        let hex = is_hex64(event_id);
        if !memory_slug(slug) || !hex || !slugs.insert(slug.to_owned()) {
            return Err(bad());
        }
        bytes += json!({"slug": slug, "body": body, "eventId": event_id, "createdAt": created_at})
            .to_string()
            .len();
        if bytes > MEMORY_TEXT_BYTES {
            return Err(MemoryError::Failed("Memory listing exceeds budget".into()));
        }
        listing.entries.push(MemoryEntry {
            slug: slug.to_owned(),
            body: body.to_owned(),
            event_id: event_id.to_owned(),
            created_at,
        });
    }
    listing.entries.sort_by(|a, b| slug_order(&a.slug, &b.slug));
    Ok(listing)
}

/// Slugs as buzz-app's `localeCompare` orders them: `_`, `-`, `/`,
/// digits, then letters regardless of case, and then lowercase first.
fn slug_order(a: &str, b: &str) -> std::cmp::Ordering {
    let primary = |slug: &str| -> Vec<u32> {
        slug.chars()
            .map(|c| match c {
                '_' => 0,
                '-' => 1,
                '/' => 2,
                '0'..='9' => 3 + c as u32 - '0' as u32,
                c => 16 + c.to_ascii_lowercase() as u32,
            })
            .collect()
    };
    primary(a).cmp(&primary(b)).then_with(|| {
        let case =
            |slug: &str| -> Vec<bool> { slug.chars().map(|c| c.is_ascii_uppercase()).collect() };
        case(a).cmp(&case(b))
    })
}

/// NIP-AE slugs: `core`, or `mem/` and lowercase path parts (`memorySlug`).
fn memory_slug(slug: &str) -> bool {
    if slug == "core" {
        return true;
    }
    let Some(path) = slug.strip_prefix("mem/") else {
        return false;
    };
    slug.len() <= 255
        && path.split('/').all(|part| {
            !part.is_empty()
                && part.len() <= 64
                && part.bytes().enumerate().all(|(index, b)| {
                    b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || (index > 0 && (b == b'-' || b == b'_'))
                })
        })
}

impl Session {
    /// Reads `agent`'s memories addressed to the viewer. The caller shows
    /// them to the agent's verified owner only.
    pub async fn agent_memories(&self, agent: &str) -> Result<MemoryListing, MemoryError> {
        if !memory_agent(agent, &self.viewer) {
            return Err(MemoryError::Failed("Invalid memory target".into()));
        }
        let http = self.direct.http.clone();
        let identity = self.direct.identity.clone();
        let agent = agent.to_owned();
        let viewer = self.viewer.clone();
        let read = async move {
            let filter = json!([{
                "kinds": [30174],
                "authors": [agent],
                "#p": [viewer],
                "limit": MEMORY_EVENTS,
            }]);
            let body = match http.post("/query", filter.to_string()).await {
                Ok(body) => body,
                Err(HttpError::Status {
                    status: 401 | 403, ..
                }) => return Err(MemoryError::Denied),
                Err(_) => return Err(MemoryError::Failed("Memory read failed".into())),
            };
            if body.len() > MEMORY_WIRE_BYTES {
                return Err(MemoryError::Failed("Memory read failed".into()));
            }
            let body = String::from_utf8(body)
                .map_err(|_| MemoryError::Failed("Memory read failed".into()))?;
            let records = buzz_desktop::relay::agent::memory_wire_records(&body)
                .map_err(MemoryError::Failed)?;
            let decoded = identity
                .decode_agent_memories(&agent, &records)
                .map_err(MemoryError::Failed)?;
            memory_listing(&decoded)
        };
        self.runtime
            .spawn(async move {
                tokio::time::timeout(MEMORY_TIMEOUT, read)
                    .await
                    .unwrap_or_else(|_| Err(MemoryError::Failed("Memory read failed".into())))
            })
            .await
            .unwrap_or_else(|_| Err(MemoryError::Failed("The session has closed".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listings_are_validated_and_sorted() {
        let id = "a".repeat(64);
        let value = json!({
            "partial": false,
            "entries": [
                {"slug": "mem/zeta", "body": "z", "eventId": id, "createdAt": 2},
                {"slug": "core", "body": "c", "eventId": id, "createdAt": 1},
            ],
        });
        let listing = memory_listing(&value).unwrap();
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| e.slug.as_str())
                .collect::<Vec<_>>(),
            ["core", "mem/zeta"]
        );
        let duplicate = json!({
            "partial": false,
            "entries": [
                {"slug": "core", "body": "c", "eventId": id, "createdAt": 1},
                {"slug": "core", "body": "c", "eventId": id, "createdAt": 1},
            ],
        });
        assert!(memory_listing(&duplicate).is_err());
        let bad_slug = json!({
            "partial": false,
            "entries": [{"slug": "mem/Bad", "body": "", "eventId": id, "createdAt": 1}],
        });
        assert!(memory_listing(&bad_slug).is_err());
    }

    #[test]
    fn listings_sort_as_locale_compare() {
        let id = "a".repeat(64);
        let entry = |slug: &str| json!({"slug": slug, "body": "", "eventId": id, "createdAt": 1});
        let value = json!({
            "partial": false,
            "entries": [
                entry("mem/a1"),
                entry("mem/a/b"),
                entry("mem/b"),
                entry("mem/a-b"),
                entry("core"),
                entry("mem/a_b"),
                entry("mem/a"),
            ],
        });
        let listing = memory_listing(&value).unwrap();
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| e.slug.as_str())
                .collect::<Vec<_>>(),
            [
                "core", "mem/a", "mem/a_b", "mem/a-b", "mem/a/b", "mem/a1", "mem/b"
            ]
        );
    }

    #[test]
    fn memory_targets_are_other_keys() {
        let viewer = "b".repeat(64);
        assert!(memory_agent(&"a".repeat(64), &viewer));
        assert!(!memory_agent(&viewer, &viewer));
        assert!(!memory_agent("short", &viewer));
    }
}
