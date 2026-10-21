//! The owner's agent list (buzz-app's kind 30177 records, #870): every
//! agent the owner has is listed in an owner-signed record with its type
//! as the `runtime` tag, so each of the owner's devices can show the
//! agents that run elsewhere. Records without a runtime belong to earlier
//! systems and stay hidden.
//!
//! Another desktop applies a record whole to an agent it has with that
//! key, and its parser requires `parallelism` and `respond_to`, so a
//! record keeps every setting and tag of the one it replaces and defaults
//! those two (buzz-app's docs/relay-queries.md, "Managed-agent
//! registration").

use buzz_identity::{Identity, Purpose};
use buzz_relay_client::RelayHttp;
use buzz_relay_client::socket::Filter;
use nostr::prelude::Event;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;

pub const KIND: u16 = 30177;
/// The most records one read asks for.
const READ_LIMIT: usize = 500;

/// The content and tags of a record as read or written.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RecordBody {
    pub content: Map<String, Value>,
    pub tags: Vec<Vec<String>>,
}

/// One of the viewer's records.
#[derive(Clone, Debug, PartialEq)]
pub struct Listing {
    pub pubkey: String,
    pub name: String,
    /// An HTTPS image URL.
    pub picture: Option<String>,
    /// The agent type's key, `<plugin>/<type>`; `None` for earlier
    /// systems' records.
    pub runtime: Option<String>,
    /// The record as read or written, kept by the next one written.
    pub prior: Option<RecordBody>,
}

/// How far a community's list read got (`AgentLibrarySnapshot.status`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListingRead {
    /// No session to read it with.
    Unavailable,
    /// Not read yet.
    Idle,
    Loading,
    Ready,
    /// The last read failed, and why.
    Error(String),
}

/// One of the owner's agents in the selected community that is not on
/// any connected host: it runs on another of the owner's devices, so it
/// is shown read-only and never run here.
#[derive(Clone, Debug, PartialEq)]
pub struct Elsewhere {
    pub pubkey: String,
    pub name: String,
    pub picture: Option<String>,
    /// The agent type's key, as its record's `runtime`.
    pub agent_type: String,
    /// The canonical `wss://` community origin.
    pub relay: String,
    pub owner: String,
}

fn runtime_ok(value: &str) -> bool {
    let part = |part: &str| {
        let mut bytes = part.bytes();
        bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && bytes.all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
            })
    };
    matches!(value.split_once('/'), Some((plugin, kind)) if part(plugin) && part(kind))
}

fn is_pubkey(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A picture as a record carries it: an HTTPS image URL, never inline
/// data (buzz-app's `recordPicture` over `avatarSource`).
pub fn record_picture(value: Option<&str>) -> Option<String> {
    let source = value?.trim();
    if source.len() > 2048 {
        return None;
    }
    let url = url::Url::parse(source).ok()?;
    (url.scheme() == "https" && url.username().is_empty() && url.password().is_none())
        .then(|| url.to_string())
}

fn tags_of(event: &Event) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect()
}

fn tag<'a>(tags: &'a [Vec<String>], name: &str) -> Option<&'a str> {
    tags.iter()
        .find(|tag| tag.first().map(String::as_str) == Some(name))
        .and_then(|tag| tag.get(1))
        .map(String::as_str)
}

/// The newest record of each agent, read as listings.
pub fn listings_of(events: &[Event], viewer: &str) -> Vec<Listing> {
    let mut heads: HashMap<String, &Event> = HashMap::new();
    let mut order = Vec::new();
    for event in events {
        if event.kind.as_u16() != KIND || event.pubkey.to_hex() != viewer {
            continue;
        }
        let tags = tags_of(event);
        let d = tag(&tags, "d").unwrap_or_default().to_owned();
        match heads.get(&d) {
            Some(current)
                if event.created_at < current.created_at
                    || (event.created_at == current.created_at && event.id >= current.id) => {}
            Some(_) => {
                heads.insert(d, event);
            }
            None => {
                order.push(d.clone());
                heads.insert(d, event);
            }
        }
    }
    order
        .into_iter()
        .filter(|pubkey| is_pubkey(pubkey))
        .map(|pubkey| {
            let event = heads[&pubkey];
            let tags = tags_of(event);
            let runtime = tag(&tags, "runtime")
                .filter(|runtime| runtime_ok(runtime))
                .map(str::to_owned);
            let content = match serde_json::from_str::<Value>(&event.content) {
                Ok(Value::Object(content)) => content,
                _ => Map::new(),
            };
            let name = content
                .get("name")
                .and_then(Value::as_str)
                .map(|name| name.trim().to_owned())
                .unwrap_or_default();
            let picture = record_picture(content.get("picture").and_then(Value::as_str));
            Listing {
                pubkey,
                name,
                picture,
                runtime,
                prior: Some(RecordBody { content, tags }),
            }
        })
        .collect()
}

/// The record that lists `entry`, keeping every setting and tag of the
/// one it replaces (`entry.prior`) but its picture.
pub fn record(entry: &Listing, runtime: &str) -> RecordBody {
    let mut content = Map::new();
    content.insert("parallelism".into(), json!(1));
    content.insert("respond_to".into(), json!("owner-only"));
    if let Some(prior) = &entry.prior {
        for (field, value) in &prior.content {
            if field != "picture" {
                content.insert(field.clone(), value.clone());
            }
        }
    }
    content.insert("name".into(), json!(entry.name));
    if let Some(picture) = &entry.picture {
        content.insert("picture".into(), json!(picture));
    }
    let mut tags = vec![
        vec!["d".to_owned(), entry.pubkey.clone()],
        vec!["runtime".to_owned(), runtime.to_owned()],
    ];
    if let Some(prior) = &entry.prior {
        tags.extend(
            prior
                .tags
                .iter()
                .filter(|tag| {
                    !matches!(
                        tag.first().map(String::as_str),
                        Some("d" | "runtime" | "client-id")
                    )
                })
                .cloned(),
        );
    }
    RecordBody { content, tags }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Signs and posts an owner event, resolving once the relay accepts it.
async fn post(
    http: &RelayHttp,
    owner: &Identity,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
    failed: &str,
) -> Result<(), String> {
    let unsigned = owner
        .template(Purpose::Content, kind, now(), tags, content)
        .map_err(|error| error.to_string())?;
    let signed = owner
        .sign(Purpose::Content, unsigned)
        .map_err(|error| error.to_string())?;
    let body = serde_json::to_string(&signed).map_err(|error| error.to_string())?;
    let receipt = http
        .post("/events", body)
        .await
        .map_err(|_| failed.to_owned())?;
    let receipt: Value = serde_json::from_slice(&receipt).unwrap_or(Value::Null);
    if receipt.get("accepted").and_then(Value::as_bool) == Some(false) {
        return Err(failed.to_owned());
    }
    Ok(())
}

/// Reads the viewer's records in the community.
pub async fn read(http: &RelayHttp, viewer: &str) -> Result<Vec<Listing>, String> {
    let filter: Filter = json!({ "kinds": [KIND], "authors": [viewer], "limit": READ_LIMIT })
        .as_object()
        .cloned()
        .unwrap_or_default();
    let events = http
        .query(&[filter])
        .await
        .map_err(|error| error.to_string())?;
    Ok(listings_of(&events, viewer))
}

/// Publishes the viewer's record of an agent, replacing any before it, and
/// resolves with what it wrote once the relay accepts it.
pub async fn publish(
    http: &RelayHttp,
    owner: &Arc<Identity>,
    entry: &Listing,
    runtime: &str,
) -> Result<RecordBody, String> {
    let body = record(entry, runtime);
    post(
        http,
        owner,
        KIND,
        body.tags.clone(),
        Value::Object(body.content.clone()).to_string(),
        "The agent list was not updated. Retry.",
    )
    .await?;
    Ok(body)
}

/// NIP-09 coordinate deletion of the viewer's own record of `agent`.
pub async fn delete(http: &RelayHttp, owner: &Arc<Identity>, agent: &str) -> Result<(), String> {
    let coordinate = format!("{KIND}:{}:{agent}", owner.pubkey_hex());
    post(
        http,
        owner,
        5,
        vec![vec!["a".to_owned(), coordinate]],
        String::new(),
        "Record deletion is not confirmed. Retry.",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};

    fn event(keys: &Keys, d: &str, at: u64, content: Value, extra: &[&[&str]]) -> Event {
        let mut tags = vec![Tag::parse(["d", d]).unwrap()];
        for tag in extra {
            tags.push(Tag::parse(tag.iter().copied()).unwrap());
        }
        EventBuilder::new(Kind::from_u16(KIND), content.to_string())
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(keys)
            .unwrap()
    }

    #[test]
    fn reads_the_newest_record_of_each_agent() {
        let keys = Keys::generate();
        let viewer = keys.public_key().to_hex();
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let events = vec![
            event(
                &keys,
                &a,
                1,
                json!({"name": "Old"}),
                &[&["runtime", "buzz.claude/claude"]],
            ),
            event(
                &keys,
                &a,
                2,
                json!({"name": " Ada ", "picture": "https://x.example/a.png", "parallelism": 3}),
                &[&["runtime", "buzz.claude/claude"], &["extra", "1"]],
            ),
            event(&keys, &b, 1, json!({"name": "Legacy"}), &[]),
            event(&keys, &b.replace('b', "B"), 1, json!({"name": "Bad"}), &[]),
            event(
                &Keys::generate(),
                &a,
                9,
                json!({"name": "Other owner"}),
                &[],
            ),
            event(
                &keys,
                &"c".repeat(64),
                1,
                json!({"name": "Bad runtime", "picture": "data:image/png;base64,AA=="}),
                &[&["runtime", "Nope"]],
            ),
        ];
        let listings = listings_of(&events, &viewer);
        assert_eq!(listings.len(), 3);
        assert_eq!(listings[0].name, "Ada");
        assert_eq!(
            listings[0].picture.as_deref(),
            Some("https://x.example/a.png")
        );
        assert_eq!(listings[0].runtime.as_deref(), Some("buzz.claude/claude"));
        assert_eq!(listings[1].runtime, None);
        assert_eq!(listings[2].runtime, None);
        assert_eq!(listings[2].picture, None);
    }

    #[test]
    fn a_record_keeps_every_setting_and_tag_but_the_picture() {
        let prior = RecordBody {
            content: json!({"parallelism": 4, "respond_to": "anyone", "model": "m", "picture": "https://old"})
                .as_object()
                .cloned()
                .unwrap(),
            tags: vec![
                vec!["d".into(), "x".into()],
                vec!["runtime".into(), "old/type".into()],
                vec!["client-id".into(), "1".into()],
                vec!["extra".into(), "kept".into()],
            ],
        };
        let entry = Listing {
            pubkey: "a".repeat(64),
            name: "Ada".into(),
            picture: None,
            runtime: None,
            prior: Some(prior),
        };
        let body = record(&entry, "buzz.codex/codex");
        assert_eq!(
            Value::Object(body.content),
            json!({"parallelism": 4, "respond_to": "anyone", "model": "m", "name": "Ada"})
        );
        assert_eq!(
            body.tags,
            vec![
                vec!["d".to_owned(), "a".repeat(64)],
                vec!["runtime".to_owned(), "buzz.codex/codex".to_owned()],
                vec!["extra".to_owned(), "kept".to_owned()],
            ]
        );
        let fresh = record(
            &Listing {
                prior: None,
                picture: Some("https://x/p.png".into()),
                ..entry
            },
            "buzz.codex/codex",
        );
        assert_eq!(
            Value::Object(fresh.content),
            json!({"parallelism": 1, "respond_to": "owner-only", "name": "Ada", "picture": "https://x/p.png"})
        );
    }
}
