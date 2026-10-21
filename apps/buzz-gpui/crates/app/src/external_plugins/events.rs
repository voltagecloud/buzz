//! Raw Nostr events for plugins (buzz-app's `session.read`, `observe` and
//! `outbox.send`): reading the selected community as the user, following
//! live events that match filters, and publishing events of the kinds the
//! app itself writes. Publishing needs the `publish-events` permission.

use super::capabilities::{Failure, Watch, community, on_tokio};
use super::protocol::INVALID_PARAMS;
use crate::community::{AppState, IdentityState};
use crate::runtime::Runtime;
use buzz_identity::{Identity, Purpose};
use buzz_relay_client::RelayHttp;
use gpui_kit::*;
use nostr::prelude::Event;
use serde_json::{Map, Value, json};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

const MAX_FILTERS: usize = 10;
/// Most events one `events/query` answer carries.
const MAX_EVENTS: usize = 500;
const MAX_TAGS: usize = 2_000;
/// Event ids a watch remembers, so overlapping routes send each once.
const SEEN: usize = 4_096;

fn invalid(message: impl Into<String>) -> Failure {
    (INVALID_PARAMS, message.into())
}

fn identity(cx: &App) -> Result<Arc<Identity>, Failure> {
    match &cx.global::<AppState>().identity {
        IdentityState::Ready(identity) => Ok(identity.clone()),
        _ => Err((1, "You are not signed in.".to_owned())),
    }
}

/// The `filters` param: NIP-01 filters of `ids`, `authors`, `kinds`,
/// `#<letter>` tags, `since`, `until` and `limit`, and with `extended`,
/// the relay's read extensions (`ReadFilter`): search, the home feed and
/// paged history and threads.
fn filters(params: &Value, extended: bool) -> Result<Vec<Map<String, Value>>, Failure> {
    let list = params
        .get("filters")
        .and_then(Value::as_array)
        .filter(|list| (1..=MAX_FILTERS).contains(&list.len()))
        .ok_or_else(|| invalid(format!("filters must list 1 to {MAX_FILTERS} filters")))?;
    list.iter()
        .map(|filter| {
            let filter = filter
                .as_object()
                .ok_or_else(|| invalid("a filter must be an object"))?;
            for (key, value) in filter {
                let valid = match key.as_str() {
                    "ids" | "authors" => strings(value).is_some(),
                    "kinds" => value
                        .as_array()
                        .is_some_and(|kinds| kinds.iter().all(|kind| kind.as_u64().is_some())),
                    "since" | "until" | "limit" => value.as_u64().is_some(),
                    "top_level" | "include_aux" | "include_summaries" | "thread_window"
                        if extended =>
                    {
                        value.is_boolean()
                    }
                    "search" | "before_id" | "thread_cursor_id" if extended => {
                        value.as_str().is_some_and(|text| text.len() <= 512)
                    }
                    "search_mode" if extended => {
                        matches!(value.as_str(), Some("prefix" | "fulltext"))
                    }
                    // A read the relay must answer from its primary.
                    "consistency" if extended => value.as_str() == Some("strong"),
                    "page" | "depth_limit" | "thread_cursor" if extended => {
                        value.as_u64().is_some()
                    }
                    "feed_types" if extended => strings(value).is_some(),
                    tag => {
                        tag.len() == 2
                            && tag.starts_with('#')
                            && tag.as_bytes()[1].is_ascii_alphabetic()
                            && strings(value).is_some()
                    }
                };
                if !valid {
                    return Err(invalid(format!("a filter's {key} is not valid")));
                }
            }
            Ok(filter.clone())
        })
        .collect()
}

fn strings(value: &Value) -> Option<Vec<&str>> {
    value.as_array()?.iter().map(Value::as_str).collect()
}

/// Whether `event` matches `filter`, as a relay decides (NIP-01).
/// [`matches`] for an event as JSON, signed or not yet.
fn matches_json(filter: &Map<String, Value>, event: &Value) -> bool {
    let has =
        |value: &Value, wanted: &str| strings(value).is_some_and(|list| list.contains(&wanted));
    let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
    let created_at = event.get("created_at").and_then(Value::as_u64).unwrap_or(0);
    filter.iter().all(|(key, value)| match key.as_str() {
        "ids" => has(value, text("id")),
        "authors" => has(value, text("pubkey")),
        "kinds" => value.as_array().is_some_and(|kinds| {
            kinds
                .iter()
                .any(|kind| kind.as_u64() == event.get("kind").and_then(Value::as_u64))
        }),
        "since" => value.as_u64().is_none_or(|since| created_at >= since),
        "until" => value.as_u64().is_none_or(|until| created_at <= until),
        tag if tag.starts_with('#') => event["tags"].as_array().is_some_and(|tags| {
            tags.iter().any(|t| {
                t.get(0).and_then(Value::as_str) == Some(&tag[1..])
                    && t.get(1)
                        .and_then(Value::as_str)
                        .is_some_and(|v| has(value, v))
            })
        }),
        "limit" | "consistency" => true,
        _ => false,
    })
}

pub fn matches(filter: &Map<String, Value>, event: &Event) -> bool {
    let has =
        |value: &Value, wanted: &str| strings(value).is_some_and(|list| list.contains(&wanted));
    filter.iter().all(|(key, value)| match key.as_str() {
        "ids" => has(value, &event.id.to_hex()),
        "authors" => has(value, &event.pubkey.to_hex()),
        "kinds" => value.as_array().is_some_and(|kinds| {
            kinds
                .iter()
                .any(|kind| kind.as_u64() == Some(u64::from(event.kind.as_u16())))
        }),
        "since" => value
            .as_u64()
            .is_none_or(|since| event.created_at.as_secs() >= since),
        "until" => value
            .as_u64()
            .is_none_or(|until| event.created_at.as_secs() <= until),
        tag if tag.starts_with('#') => {
            let name = &tag[1..];
            event.tags.iter().any(|t| {
                let t = t.as_slice();
                t.first().is_some_and(|first| first == name)
                    && t.get(1).is_some_and(|tag_value| has(value, tag_value))
            })
        }
        "limit" | "consistency" => true,
        // Search, feeds and the thread extensions are the relay's to
        // decide: no event here matches them (`matchesEvent`).
        _ => false,
    })
}

/// `events/query`: events matching `filters`, read from the community as
/// the user.
pub fn query(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let filters = filters(params, true)?;
    let http = RelayHttp::new(community.read(cx).origin.clone(), identity(cx)?);
    let session = community.read(cx).session.clone();
    // The channels you can read, for the outbox's events.
    let readable: HashSet<String> = community
        .read(cx)
        .snapshot
        .channels
        .iter()
        .map(|channel| channel.id.clone())
        .collect();
    Ok(on_tokio(cx, async move {
        let mut events = http
            .query_raw(&filters)
            .await
            .map_err(|error| (2, error.to_string()))?;
        // Only signed events that check out, as `session.read` verifies.
        let verifier = buzz_relay_client::Verifier::new(4);
        let mut discovery = Vec::new();
        events.retain(|value| {
            let Some(verified) = serde_json::from_value::<Event>(value.clone())
                .ok()
                .and_then(|event| verifier.verify(event).ok())
            else {
                return false;
            };
            use buzz_relay_client::kinds::{CHANNEL_MEMBERS, CHANNEL_METADATA};
            if matches!(verified.kind_u16(), CHANNEL_METADATA | CHANNEL_MEMBERS) {
                discovery.push(verified);
            }
            true
        });
        // A message search across channels resolves the channels its
        // results are in first, open ones you have not joined included,
        // and keeps only results in exactly one that resolved
        // (`readVerified`).
        let message_kinds = |filter: &Map<String, Value>| {
            filter
                .get("kinds")
                .and_then(Value::as_array)
                .is_some_and(|kinds| {
                    !kinds.is_empty()
                        && kinds
                            .iter()
                            .all(|kind| matches!(kind.as_u64(), Some(9 | 40002 | 40008)))
                })
        };
        let across = filters.iter().all(|filter| {
            ["search", "authors", "since", "until"]
                .iter()
                .any(|key| filter.contains_key(*key))
                && !filter.contains_key("#h")
                && message_kinds(filter)
        });
        if across {
            let single = |event: &Value| -> Option<String> {
                let mut channels =
                    event["tags"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|tag| {
                            (tag.get(0).and_then(Value::as_str) == Some("h"))
                                .then(|| tag.get(1).and_then(Value::as_str).map(str::to_owned))
                                .flatten()
                        });
                let first = channels.next()?;
                channels.next().is_none().then_some(first)
            };
            let mut ids: Vec<String> = events.iter().filter_map(single).collect();
            ids.sort();
            ids.dedup();
            // One lookup: too many channels is the read's error, as
            // buzz-app's `resolve` refuses past its limit.
            let resolved: HashSet<String> = if ids.is_empty() {
                HashSet::new()
            } else {
                session
                    .resolve_channels(ids)
                    .await
                    .map_err(|error| (2, error))?
                    .into_iter()
                    .map(|channel| channel.id)
                    .collect()
            };
            events.retain(|event| single(event).is_some_and(|id| resolved.contains(&id)));
        }
        // Only from channels you can read (`eventVisibility`): each `h`,
        // and a roster or admin list's `d`. The read's own channel
        // metadata and rosters count first, as `session.read` accepts
        // them into discovery (`acceptDiscovery`), so one may introduce
        // an open channel or take one away.
        let named = |event: &Value| -> Vec<String> {
            let kind = event.get("kind").and_then(Value::as_u64).unwrap_or(0);
            event["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tag| {
                    let name = tag.get(0).and_then(Value::as_str)?;
                    let value = tag.get(1).and_then(Value::as_str)?;
                    (name == "h" || (name == "d" && matches!(kind, 39001 | 39002)))
                        .then(|| value.to_owned())
                })
                .collect()
        };
        let mut channels: Vec<String> = events.iter().flat_map(named).collect();
        channels.sort();
        channels.dedup();
        session.accept_discovery(discovery).await;
        if !channels.is_empty() {
            let open: HashSet<String> = session
                .accessible_channels(channels)
                .await
                .into_iter()
                .collect();
            events.retain(|event| named(event).iter().all(|id| open.contains(id)));
        }
        // Your own events count too, as `session.read` merges what the
        // outbox holds: those on their way and those it saw delivered
        // (`local`).
        if let Some(outbox) = session.outbox().await {
            let known: HashSet<String> = events
                .iter()
                .filter_map(|event| event.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect();
            for op in outbox.visible().iter() {
                // A send still waiting for its slot is not signed yet; it
                // counts all the same (`operation.event`).
                let value = match &op.signed {
                    Some(signed) => serde_json::to_value(signed),
                    None => serde_json::to_value(&op.event),
                };
                let Ok(mut value) = value else {
                    continue;
                };
                // Product feedback never shows, and nothing from a
                // channel you cannot read (`session.read`).
                let channel = value["tags"]
                    .as_array()
                    .and_then(|tags| {
                        tags.iter()
                            .find(|tag| tag.get(0).and_then(Value::as_str) == Some("h"))
                    })
                    .and_then(|tag| tag.get(1))
                    .and_then(Value::as_str);
                if value["kind"].as_u64() == Some(42000)
                    || channel.is_some_and(|channel| !readable.contains(channel))
                {
                    continue;
                }
                let id = op.id().to_hex();
                if !filters.iter().any(|filter| matches_json(filter, &value)) {
                    continue;
                }
                // The relay's copy carries how it is going too
                // (`projectEvents`).
                if known.contains(&id) {
                    if let Some(event) = events
                        .iter_mut()
                        .find(|event| event.get("id").and_then(Value::as_str) == Some(id.as_str()))
                    {
                        event["delivery"] = json!(super::outbox::delivery_name(op.delivery));
                        event["error"] = json!(op.error);
                    }
                    continue;
                }
                {
                    value["id"] = json!(id);
                    // How it is going, as the projected rows carry.
                    value["delivery"] = json!(super::outbox::delivery_name(op.delivery));
                    value["error"] = json!(op.error);
                    events.push(value);
                }
            }
        }
        // Newest first, ties by id, unless the relay ranked them (a
        // `search` or `feed_types` filter keeps its order: `projectEvents`).
        let ranked = filters
            .iter()
            .any(|filter| filter.contains_key("search") || filter.contains_key("feed_types"));
        if !ranked {
            events.sort_by(|a, b| {
                let at =
                    |event: &Value| event.get("created_at").and_then(Value::as_u64).unwrap_or(0);
                let id = |event: &Value| {
                    event
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                at(b).cmp(&at(a)).then_with(|| id(a).cmp(&id(b)))
            });
        }
        events.truncate(MAX_EVENTS);
        Ok(Value::Array(events))
    }))
}

/// `events/publish`: signs `kind`, `content` and `tags` as the user and
/// posts the event to the community. Kinds the app does not write itself
/// are refused.
pub fn publish(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let identity = identity(cx)?;
    let kind = params
        .get("kind")
        .and_then(Value::as_u64)
        .and_then(|kind| u16::try_from(kind).ok())
        .ok_or_else(|| invalid("kind is required"))?;
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tags: Vec<Vec<String>> = match params.get("tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(tags) => tags
            .as_array()
            .filter(|tags| tags.len() <= MAX_TAGS)
            .and_then(|tags| {
                tags.iter()
                    .map(|tag| {
                        strings(tag)
                            .filter(|tag| !tag.is_empty())
                            .map(|tag| tag.into_iter().map(str::to_owned).collect())
                    })
                    .collect()
            })
            .ok_or_else(|| invalid("tags must be a list of lists of strings"))?,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    // App data (NIP-78) is signed apart from what shows in conversations.
    let purpose = if kind == 30078 {
        Purpose::AppData
    } else {
        Purpose::Content
    };
    let event = identity
        .template(purpose, kind, now, tags, content)
        .and_then(|unsigned| identity.sign(purpose, unsigned))
        .map_err(|error| (3, format!("Buzz does not publish this event: {error}")))?;
    let http = RelayHttp::new(community.read(cx).origin.clone(), identity);
    Ok(on_tokio(cx, async move {
        let body = serde_json::to_string(&event).map_err(|error| (3, error.to_string()))?;
        buzz_relay_client::outbox::accept_publish(http.post("/events", body).await, &event.id)
            .map_err(|error| {
                (
                    3,
                    match error {
                        buzz_relay_client::outbox::PublishError::Rejected(message)
                        | buzz_relay_client::outbox::PublishError::Uncertain(message) => message,
                    },
                )
            })?;
        Ok(json!({ "id": event.id.to_hex(), "event": event }))
    }))
}

/// `events/watch`: sends `events/live` with each live event that matches
/// `filters`, from now on.
pub fn watch(
    params: &Value,
    notify: impl Fn(Value, &mut App) + 'static,
    cx: &mut App,
) -> Result<(Watch, Value), Failure> {
    let community = community(cx)?;
    let filters = filters(params, false)?;
    let mut live = community.read(cx).session.live_events();
    let (sender, mut values) = futures::channel::mpsc::unbounded::<(Vec<Value>, Option<String>)>();
    Runtime::handle(cx).spawn(async move {
        let mut seen = (HashSet::new(), VecDeque::new());
        while let Some(batch) = live.recv().await {
            if sender.is_closed() {
                return;
            }
            let matched: Vec<Value> = batch
                .events
                .iter()
                .filter(|event| filters.iter().any(|filter| matches(filter, event)))
                .filter(|event| {
                    let fresh = seen.0.insert(event.id);
                    if fresh {
                        seen.1.push_back(event.id);
                        if seen.1.len() > SEEN
                            && let Some(old) = seen.1.pop_front()
                        {
                            seen.0.remove(&old);
                        }
                    }
                    fresh
                })
                .filter_map(|event| serde_json::to_value(&**event).ok())
                .collect();
            if !matched.is_empty() && sender.unbounded_send((matched, batch.channel)).is_err() {
                return;
            }
        }
    });
    let forward = cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some((events, channel)) = values.next().await {
            // The channel route the batch came on, if any.
            cx.update(|cx| notify(json!({ "events": events, "channel": channel }), cx));
        }
    });
    Ok((Watch::new((), forward), json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn event(kind: u16, tags: Vec<Vec<String>>) -> Event {
        let identity = Identity::ephemeral();
        let unsigned = identity
            .template(Purpose::Content, kind, 1_000, tags, "hi".into())
            .unwrap();
        identity.sign(Purpose::Content, unsigned).unwrap()
    }

    #[test]
    fn filters_match_like_a_relay() {
        let event = event(9, vec![vec!["h".into(), "room".into()]]);
        let filter = |value: Value| {
            filters(&json!({ "filters": [value] }), false)
                .unwrap()
                .remove(0)
        };
        assert!(matches(
            &filter(json!({ "kinds": [9], "#h": ["room"] })),
            &event
        ));
        assert!(!matches(&filter(json!({ "kinds": [7] })), &event));
        assert!(!matches(&filter(json!({ "#h": ["other"] })), &event));
        assert!(matches(
            &filter(json!({ "authors": [event.pubkey.to_hex()], "since": 1_000 })),
            &event
        ));
        assert!(!matches(&filter(json!({ "until": 999 })), &event));
        assert!(matches(
            &filter(json!({ "ids": [event.id.to_hex()] })),
            &event
        ));
    }

    #[test]
    fn filters_are_checked() {
        for bad in [
            json!({ "filters": [] }),
            json!({ "filters": [{ "search_mode": "fuzzy" }] }),
            json!({ "filters": [{ "#hh": ["x"] }] }),
            json!({ "filters": [{ "kinds": ["9"] }] }),
            json!({ "filters": [7] }),
        ] {
            assert!(filters(&bad, true).is_err(), "{bad}");
        }
        // Search reads history; a live watch cannot apply it.
        let search = json!({ "filters": [{ "search": "x", "search_mode": "fulltext" }] });
        assert!(filters(&search, true).is_ok());
        assert!(filters(&search, false).is_err());
    }
}
