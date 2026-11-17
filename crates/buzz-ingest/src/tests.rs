//! Real HTTP journeys. Only the external relay boundary is faked.

use crate::{
    config::{Config, Secrets},
    relay::Relay,
};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, KeyInit, Mac};
use nostr::{Event, Keys};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

const SECRET: &str = "synthetic-provider-signing-secret";
const CHANNEL: &str = "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10";
/// The publisher-only channel for Linear issue route records.
const ROUTES: &str = "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a12";
/// A fault for the next record write under a coordinate prefix.
#[derive(Clone, Copy, PartialEq)]
enum RecordFault {
    /// Refuse the write.
    Refuse,
    /// Store the write, then lose its acknowledgement.
    LoseAck,
    /// Keep the current head, as after a concurrent newer write, and report
    /// the write as a duplicate.
    Outrun,
    /// Fail the request but keep the write, which [`land`] commits later, as
    /// when the relay commits it after the publisher gave up.
    Hold,
}
#[derive(Default)]
struct Store {
    events: BTreeMap<String, Event>,
    heads: BTreeMap<String, String>,
    fail_queries: usize,
    fail_reply: usize,
    record_fault: Option<(&'static str, RecordFault)>,
    held: Vec<Event>,
    fail_event_lookup: Option<String>,
    lose_root_response: bool,
    submissions: Vec<Event>,
    queries: usize,
    delay_ms: u64,
    submit_delay_ms: u64,
    /// Channels the publisher was removed from: their events are hidden
    /// from it, and it cannot write there.
    removed: std::collections::BTreeSet<String>,
    /// A channel whose membership returns right after the next query.
    restore_after_query: Option<String>,
    /// Channels that, like NIP-CL system channels, a filter reads only when
    /// it names them in `#h`.
    named_only: std::collections::BTreeSet<String>,
}
type Shared = Arc<Mutex<Store>>;
fn tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let p = tag.as_slice();
        if p.first().map(String::as_str) == Some(name) {
            p.get(1).map(String::as_str)
        } else {
            None
        }
    })
}
fn auth(headers: &HeaderMap, body: &[u8]) {
    let raw = headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .strip_prefix("Nostr ")
        .unwrap();
    let event: Event = serde_json::from_slice(&STANDARD.decode(raw).unwrap()).unwrap();
    event.verify().unwrap();
    assert_eq!(event.kind.as_u16(), 27235);
    assert_eq!(
        tag(&event, "payload"),
        Some(hex::encode(Sha256::digest(body)).as_str())
    );
    assert_eq!(tag(&event, "method"), Some("POST"));
}
async fn query(
    State(store): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    auth(&headers, &body);
    let delay = store.lock().unwrap().delay_ms;
    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    let filters: Value = serde_json::from_slice(&body).unwrap();
    let filter = &filters[0];
    assert_eq!(filter["consistency"], "strong");
    let mut store = store.lock().unwrap();
    store.queries += 1;
    if store.fail_event_lookup.as_ref().is_some_and(|id| {
        filter
            .get("ids")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.contains(&json!(id)))
    }) {
        store.fail_event_lookup = None;
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    if store.fail_queries > 0 {
        store.fail_queries -= 1;
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    let mut events = Vec::new();
    for event in store.events.values() {
        if event.kind.as_u16() == buzz_core::kind::KIND_INTEGRATION_RECORD as u16
            && tag(event, "d").is_some_and(|d| store.heads.get(d) != Some(&event.id.to_hex()))
        {
            continue;
        }
        if tag(event, "h").is_some_and(|h| {
            store.removed.contains(h)
                || (store.named_only.contains(h)
                    && !filter["#h"]
                        .as_array()
                        .is_some_and(|named| named.contains(&json!(h))))
        }) {
            continue;
        }
        let matches = filter["kinds"]
            .as_array()
            .unwrap()
            .contains(&json!(event.kind.as_u16()))
            && filter
                .get("ids")
                .and_then(Value::as_array)
                .is_none_or(|ids| ids.contains(&json!(event.id.to_hex())))
            && filter
                .get("authors")
                .and_then(Value::as_array)
                .is_none_or(|ids| ids.contains(&json!(event.pubkey.to_hex())))
            && ["h", "d"].iter().all(|name| {
                filter
                    .get(format!("#{name}"))
                    .and_then(Value::as_array)
                    .is_none_or(|values| {
                        tag(event, name).is_some_and(|v| values.contains(&json!(v)))
                    })
            });
        if matches {
            events.push(event.clone());
        }
    }
    events.truncate(filter["limit"].as_u64().unwrap_or(2) as usize);
    if let Some(channel) = store.restore_after_query.take() {
        store.removed.remove(&channel);
    }
    (StatusCode::OK, Json(json!(events)))
}
/// Apply the relay's addressable replacement: the newest record wins, a
/// same-second tie keeps the lowest ID, and a losing write is not stored.
/// A route record instead wins only with a greater route, by the relay's own
/// ordering.
fn commit(store: &mut Store, event: &Event) -> bool {
    use buzz_core::integration::{is_route_coordinate, route};
    let id = event.id.to_hex();
    if store.events.contains_key(&id) {
        return false;
    }
    if let Some(d) = tag(event, "d") {
        let keeps_head = store
            .heads
            .get(d)
            .and_then(|head| store.events.get(head))
            .is_some_and(|head| {
                if is_route_coordinate(d) {
                    return route(&event.tags).ok() <= route(&head.tags).ok();
                }
                head.created_at > event.created_at
                    || (head.created_at == event.created_at && head.id.to_hex() < id)
            });
        if keeps_head {
            return false;
        }
        store.heads.insert(d.to_owned(), id.clone());
    }
    store.events.insert(id, event.clone());
    true
}
/// Commit the writes that [`RecordFault::Hold`] kept back.
fn land(store: &Shared) {
    let mut store = store.lock().unwrap();
    for event in std::mem::take(&mut store.held) {
        commit(&mut store, &event);
    }
}
async fn submit(
    State(store): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    auth(&headers, &body);
    let delay = {
        let store = store.lock().unwrap();
        store.delay_ms + store.submit_delay_ms
    };
    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    let event: Event = serde_json::from_slice(&body).unwrap();
    event.verify().unwrap();
    let mut store = store.lock().unwrap();
    store.submissions.push(event.clone());
    let id = event.id.to_hex();
    let unavailable = (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    if tag(&event, "h").is_some_and(|h| store.removed.contains(h)) {
        return (
            StatusCode::FORBIDDEN,
            Json(
                json!({"event_id":id,"accepted":false,"message":"restricted: not a channel member"}),
            ),
        );
    }
    let duplicate = (
        StatusCode::OK,
        Json(json!({"event_id":id,"accepted":true,"message":"duplicate:"})),
    );
    let faulted = event.kind.as_u16() == buzz_core::kind::KIND_INTEGRATION_RECORD as u16
        && store
            .record_fault
            .is_some_and(|(prefix, _)| tag(&event, "d").is_some_and(|d| d.starts_with(prefix)));
    let fault = faulted
        .then(|| store.record_fault.take())
        .flatten()
        .map(|(_, fault)| fault);
    match fault {
        Some(RecordFault::Refuse) => return unavailable,
        Some(RecordFault::Outrun) => return duplicate,
        Some(RecordFault::Hold) => {
            store.held.push(event);
            return unavailable;
        }
        Some(RecordFault::LoseAck) | None => {}
    }
    if event.kind.as_u16() == 9 {
        // Model relay timestamp admission. A stale unaccepted saved plan must
        // be refreshed, while an accepted old event is reused by exact lookup.
        if nostr::Timestamp::now()
            .as_secs()
            .abs_diff(event.created_at.as_secs())
            > 900
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!(["OK", event.id, false, "timestamp"])),
            );
        }
        let record_count = store
            .heads
            .values()
            .filter_map(|id| store.events.get(id))
            .filter(|record| tag(record, "message") == Some(event.id.to_hex().as_str()))
            .count();
        assert!(
            record_count >= 1,
            "every message requires a durable plan before publication"
        );
        if tag(&event, "e").is_some() {
            let (root, parent) = buzz_core::nip10::parse_thread_markers(&event.tags)
                .resolve()
                .expect("display plans must be real replies, including after refresh");
            assert_eq!(root, parent);
            assert!(
                store.events.contains_key(&root),
                "reply anchor must already exist"
            );
            assert!(
                record_count >= 2,
                "delivery and body fingerprint plans precede replies"
            );
            if store.fail_reply > 0 {
                store.fail_reply -= 1;
                return unavailable;
            }
        }
    }
    if !commit(&mut store, &event) {
        return duplicate;
    }
    if event.kind.as_u16() == 9 && tag(&event, "e").is_none() && store.lose_root_response {
        store.lose_root_response = false;
        return unavailable;
    }
    if fault == Some(RecordFault::LoseAck) {
        return unavailable;
    }
    (
        StatusCode::OK,
        Json(json!({"event_id":id,"accepted":true,"message":""})),
    )
}
async fn external() -> (String, Shared, tokio::task::JoinHandle<()>) {
    let store = Shared::default();
    // The route channel may be a system channel.
    store.lock().unwrap().named_only.insert(ROUTES.into());
    let router = Router::new()
        .route("/query", post(query))
        .route("/events", post(submit))
        .with_state(store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), store, task)
}
fn config(url: &str) -> Config {
    serde_json::from_value(json!({"relay_url":url,"github_repos":{"voltagecloud/buzz":CHANNEL},"linear_teams":{"TEAM":CHANNEL},"linear_webhooks":{"HOOK":"0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a11"},"linear_labels":{"LABEL-AGENT":"agent"},"linear_route_channel":ROUTES,"slack_channels":{"T123/C123":CHANNEL},"slack_workspace_url":"https://voltage.slack.com"})).unwrap()
}
fn app(config: Config, keys: &Keys) -> Router {
    crate::http::router(
        config.clone(),
        Secrets {
            github: Some(SECRET.into()),
            linear: Some(SECRET.into()),
            slack: Some(SECRET.into()),
        },
        Relay::new(&config.relay_url, keys.clone()).unwrap(),
    )
}
fn mac(body: &[u8]) -> String {
    let mut h = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    h.update(body);
    hex::encode(h.finalize().into_bytes())
}
async fn request(
    app: &Router,
    provider: &str,
    payload: &Value,
    id: &str,
    bad_signature: bool,
    age: u64,
    event: &str,
) -> (StatusCode, Value) {
    let body = payload.to_string();
    let time = nostr::Timestamp::now()
        .as_secs()
        .saturating_sub(age)
        .to_string();
    let mut req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/ingest/{provider}"))
        .header("content-type", "application/json");
    let hash = if bad_signature {
        "00".repeat(32)
    } else if provider == "slack" {
        mac(format!("v0:{time}:{body}").as_bytes())
    } else {
        mac(body.as_bytes())
    };
    req = match provider {
        "github" => req
            .header("x-hub-signature-256", format!("sha256={hash}"))
            .header("x-github-delivery", id)
            .header("x-github-event", event),
        "linear" => req
            .header("linear-signature", hash)
            .header("linear-delivery", id),
        _ => req
            .header("x-slack-signature", format!("v0={hash}"))
            .header("x-slack-request-timestamp", time),
    };
    let response = app
        .clone()
        .oneshot(req.body(axum::body::Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or_else(|_| json!(String::from_utf8_lossy(&body))),
    )
}
async fn linear(router: &Router, payload: &Value, id: &str) -> StatusCode {
    request(router, "linear", payload, id, false, 0, "").await.0
}
fn messages(store: &Shared) -> Vec<Event> {
    store
        .lock()
        .unwrap()
        .events
        .values()
        .filter(|e| e.kind.as_u16() == 9)
        .cloned()
        .collect()
}
fn github() -> Value {
    json!({"action":"opened","repository":{"full_name":"voltagecloud/buzz"},"pull_request":{"id":42,"number":7,"title":"A useful change","body":"Please review","state":"open","labels":[{"name":"agent"}]},"sender":{"login":"ada"}})
}

#[tokio::test]
async fn github_retries_preserve_plans_and_do_not_duplicate_threads_or_effects() {
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let cfg = config(&url);
    let first = app(cfg.clone(), &keys);
    let payload = github();
    assert_eq!(
        request(
            &first,
            "github",
            &payload,
            "delivery-1",
            true,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(messages(&store).is_empty());
    assert_eq!(
        request(
            &first,
            "github",
            &json!({"zen":"A signed ping","hook":{},"hook_id":1}),
            "ping-1",
            false,
            0,
            "ping"
        )
        .await
        .1["ignored"],
        true
    );
    store.lock().unwrap().lose_root_response = true;
    assert_eq!(
        request(
            &first,
            "github",
            &payload,
            "delivery-1",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(messages(&store).len(), 1);
    let restarted = app(cfg, &keys);
    store.lock().unwrap().fail_reply = 1;
    assert_eq!(
        request(
            &restarted,
            "github",
            &payload,
            "delivery-1",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(store
        .lock()
        .unwrap()
        .heads
        .keys()
        .any(|k| k.starts_with("fingerprint:")));
    // A pending plan can outlive relay timestamp admission during an outage.
    // Replace its event with an old signed equivalent in both journal aliases.
    let mut old_id = None;
    {
        let mut locked = store.lock().unwrap();
        let records: Vec<_> = locked
            .heads
            .values()
            .filter_map(|id| locked.events.get(id))
            .cloned()
            .collect();
        for record in records {
            let mut plan: Value =
                serde_json::from_slice(&STANDARD.decode(&record.content).unwrap()).unwrap();
            let saved: Event = serde_json::from_value(plan["event"].clone()).unwrap();
            if tag(&saved, "e").is_none() {
                continue;
            }
            let old = nostr::EventBuilder::new(saved.kind, saved.content)
                .tags(saved.tags.to_vec())
                .custom_created_at(nostr::Timestamp::from(
                    nostr::Timestamp::now().as_secs() - 1000,
                ))
                .sign_with_keys(&keys)
                .unwrap();
            old_id = Some(old.id.to_hex());
            plan["event"] = serde_json::to_value(&old).unwrap();
            let mut tags = record.tags.to_vec();
            tags.retain(|tag| tag.as_slice()[0] != "message");
            tags.push(nostr::Tag::parse(["message", &old.id.to_hex()]).unwrap());
            let replaced = nostr::EventBuilder::new(
                record.kind,
                STANDARD.encode(serde_json::to_vec(&plan).unwrap()),
            )
            .tags(tags)
            .sign_with_keys(&keys)
            .unwrap();
            locked.heads.insert(
                tag(&replaced, "d").unwrap().to_owned(),
                replaced.id.to_hex(),
            );
            locked.events.insert(replaced.id.to_hex(), replaced);
        }
        locked.fail_event_lookup = old_id.clone();
    }
    assert!(old_id.is_some());
    assert_eq!(
        request(
            &restarted,
            "github",
            &payload,
            "delivery-1",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        messages(&store).len(),
        1,
        "lookup failure must not refresh or publish an ambiguous plan"
    );

    assert_eq!(
        request(
            &restarted,
            "github",
            &payload,
            "delivery-1",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::OK
    );
    let before = messages(&store);
    assert_eq!(before.len(), 2);
    assert_eq!(
        request(
            &restarted,
            "github",
            &payload,
            "delivery-1",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::OK
    );
    // Delivery headers are unsigned. Replaying a valid signed body under a
    // changed header must retain the same display event.
    assert_eq!(
        request(
            &restarted,
            "github",
            &payload,
            "different-header",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(messages(&store).len(), 2);
    assert_eq!(
        request(
            &restarted,
            "github",
            &payload,
            "different-header",
            false,
            0,
            "issue_comment"
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    store.lock().unwrap().fail_queries = 1;
    assert_eq!(
        request(
            &restarted,
            "github",
            &payload,
            "delivery-1",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(messages(&store).len(), 2);
    let mut review = payload.clone();
    review["action"] = json!("submitted");
    review["review"] = json!({"id":77,"state":"approved","body":"Looks good"});
    assert_eq!(
        request(
            &restarted,
            "github",
            &review,
            "review-1",
            false,
            0,
            "pull_request_review"
        )
        .await
        .0,
        StatusCode::OK
    );
    let checks = json!({"action":"completed","repository":{"full_name":"voltagecloud/buzz"},"sender":{"login":"ci"},"check_run":{"id":88,"name":"Rust tests","head_sha":"abc123","conclusion":"failure","pull_requests":[{"number":7}],"output":{"summary":"A test failed"}}});
    assert_eq!(
        request(
            &restarted,
            "github",
            &checks,
            "check-1",
            false,
            0,
            "check_run"
        )
        .await
        .0,
        StatusCode::OK
    );
    let current = messages(&store);
    assert!(current
        .iter()
        .any(|event| event.content.contains("approved")));
    assert!(current
        .iter()
        .any(|event| event.content.contains("Rust tests") && event.content.contains("failure")));
    assert_eq!(
        current
            .iter()
            .filter(|event| tag(event, "e").is_none())
            .count(),
        1
    );
    // A check linked to four PRs fans out to four threads. A retry replays
    // completed occurrences first, so each must cost two reads and no writes;
    // otherwise later threads never fit within the delivery deadline.
    let mut fan_out = checks.clone();
    fan_out["check_run"]["id"] = json!(89);
    fan_out["check_run"]["pull_requests"] =
        json!([{"number":7},{"number":8},{"number":9},{"number":10}]);
    assert_eq!(
        request(
            &restarted,
            "github",
            &fan_out,
            "check-fan-out",
            false,
            0,
            "check_run"
        )
        .await
        .0,
        StatusCode::OK
    );
    let replay = {
        let store = store.lock().unwrap();
        (store.queries, store.submissions.len())
    };
    assert_eq!(
        request(
            &restarted,
            "github",
            &fan_out,
            "check-fan-out",
            false,
            0,
            "check_run"
        )
        .await
        .0,
        StatusCode::OK
    );
    {
        let store = store.lock().unwrap();
        assert_eq!(
            store.submissions.len(),
            replay.1,
            "a completed replay writes nothing"
        );
        assert!(
            store.queries - replay.0 <= 8,
            "four completed occurrences took {} relay reads",
            store.queries - replay.0
        );
    }
    assert_eq!(
        messages(&store)
            .iter()
            .filter(|event| tag(event, "ext-id") == Some("89"))
            .count(),
        4
    );
    // A completed delivery ID reused for a different payload is refused, not
    // acknowledged as the earlier occurrence.
    let prior = messages(&store).len();
    let mut conflicting = fan_out.clone();
    conflicting["check_run"]["output"]["summary"] = json!("A different result");
    assert_eq!(
        request(
            &restarted,
            "github",
            &conflicting,
            "check-fan-out",
            false,
            0,
            "check_run"
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(messages(&store).len(), prior);
    // With 150 ms per relay request, publishing four targets one after another
    // overruns the four-second deadline on every retry. Concurrent targets
    // finish within it.
    store.lock().unwrap().delay_ms = 150;
    let mut slow = fan_out.clone();
    slow["check_run"]["id"] = json!(90);
    assert_eq!(
        request(
            &restarted,
            "github",
            &slow,
            "check-slow-relay",
            false,
            0,
            "check_run"
        )
        .await
        .0,
        StatusCode::OK
    );
    store.lock().unwrap().delay_ms = 0;
    assert_eq!(
        messages(&store)
            .iter()
            .filter(|event| tag(event, "ext-id") == Some("90"))
            .count(),
        4
    );
    // Targets publish concurrently, so a pull request listed twice is one
    // occurrence rather than two writers racing on one thread.
    let mut repeated = fan_out.clone();
    repeated["check_run"]["id"] = json!(91);
    repeated["check_run"]["pull_requests"] = json!([{"number":7},{"number":7}]);
    let (status, body) = request(
        &restarted,
        "github",
        &repeated,
        "check-repeated-pr",
        false,
        0,
        "check_run",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["occurrences"], 1);
    let mut too_many = payload.clone();
    too_many["pull_request"]["labels"] = json!((0..17)
        .map(|i| json!({"name":format!("label-{i}")}))
        .collect::<Vec<_>>());
    let prior = messages(&store).len();
    assert_eq!(
        request(
            &restarted,
            "github",
            &too_many,
            "too-many-labels",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(messages(&store).len(), prior);
    server.abort();
}

#[tokio::test]
async fn linear_and_slack_only_publish_authenticated_mapped_public_work() {
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let router = app(config(&url), &keys);
    let now = nostr::Timestamp::now().as_secs() * 1000;
    let issue = json!({"type":"Issue","action":"create","webhookTimestamp":now,"webhookId":"HOOK","url":"https://linear.app/issue/ENG-7","actor":{"name":"Ada"},"data":{"id":"issue-7","identifier":"ENG-7","teamId":"TEAM","title":"Fix it","description":"Details","stateId":"STATE-TODO","updatedAt":"2026-11-17T10:00:00.000Z"}});
    assert_eq!(
        request(&router, "linear", &issue, "linear-1", true, 0, "")
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&router, "linear", &issue, "linear-1", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    assert!(messages(&store)
        .iter()
        .any(|event| tag(event, "ext-state") == Some("STATE-TODO")));
    let mut labeled = issue.clone();
    labeled["action"] = json!("update");
    labeled["data"]["labelIds"] = json!(["LABEL-AGENT"]);
    labeled["data"]["updatedAt"] = json!("2026-11-17T10:01:00.000Z");
    labeled["updatedFrom"] = json!({"labelIds":null});
    assert_eq!(
        request(&router, "linear", &labeled, "linear-label", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    let label_event = messages(&store)
        .into_iter()
        .find(|e| tag(e, "ext-action") == Some("issue.labeled"))
        .unwrap();
    assert_eq!(tag(&label_event, "ext-added-labels"), Some("agent"));
    let mut redelivery = issue.clone();
    redelivery["webhookTimestamp"] = json!(now + 1000);
    assert_eq!(
        request(&router, "linear", &redelivery, "linear-1", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        messages(&store).len(),
        3,
        "transport timestamps must not duplicate a Linear occurrence"
    );
    let comment = json!({"type":"Comment","action":"create","webhookTimestamp":now,"url":"https://linear.app/issue/ENG-7#comment-1","actor":{"name":"Ada"},"data":{"id":"comment-1","issueId":"issue-7","body":"An update"}});
    assert_eq!(
        request(&router, "linear", &comment, "linear-2", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(messages(&store).len(), 4); // one anchor, three ordinary replies
    let mut stale = issue.clone();
    stale["webhookTimestamp"] = json!(now - 600_000);
    assert_eq!(
        request(&router, "linear", &stale, "linear-stale", false, 0, "")
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let challenge = json!({"type":"url_verification","challenge":"verify-this"});
    assert_eq!(
        request(&router, "slack", &challenge, "", true, 0, "")
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(&router, "slack", &challenge, "", false, 0, "")
            .await
            .1["challenge"],
        "verify-this"
    );
    let mut message = json!({"type":"event_callback","team_id":"T123","event_id":"Ev1","event":{"type":"message","channel_type":"im","channel":"C123","ts":"1700000000.000001","user":"U123","text":"private content"}});
    assert_eq!(
        request(&router, "slack", &message, "", false, 0, "")
            .await
            .1["occurrences"],
        0
    );
    assert_eq!(messages(&store).len(), 4);
    message["event"]["channel_type"] = json!("channel");
    message["event"]["text"] = json!("public discussion");
    assert_eq!(
        request(&router, "slack", &message, "", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(messages(&store).len(), 6);
    assert_eq!(
        request(&router, "slack", &message, "", false, 600, "")
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut huge = message;
    huge["event"]["text"] = json!("x".repeat(1024 * 1024));
    assert_eq!(
        request(&router, "slack", &huge, "", false, 0, "").await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    // An existing issue without a saved route can use an explicitly mapped
    // signed webhook ID. A known route above must retain the team channel.
    let mut orphan = comment.clone();
    orphan["webhookId"] = json!("HOOK");
    orphan["data"]["id"] = json!("orphan-comment");
    orphan["data"]["issueId"] = json!("never-seen-issue");
    assert_eq!(
        request(&router, "linear", &orphan, "linear-orphan", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    assert!(messages(&store)
        .iter()
        .any(|event| tag(event, "h") == Some("0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a11")));
    // Consumers join repeated label tags with commas. A label name that
    // contains a comma must not read as a separate `agent` label.
    let mut comma = labeled.clone();
    comma["data"]["labelIds"] = json!(["LABEL-CSV"]);
    comma["data"]["labels"] = json!([{"id":"LABEL-CSV","name":"triage,agent"}]);
    assert_eq!(
        request(&router, "linear", &comma, "linear-comma", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    let added = messages(&store)
        .iter()
        .map(|event| buzz_core::integration::ext_fields(&event.tags))
        .find_map(|fields| {
            fields
                .get("added_labels")
                .filter(|labels| labels.contains("triage"))
                .cloned()
        })
        .unwrap();
    assert_eq!(added, "triage agent");
    assert!(!format!(",{added},").contains(",agent,"));
    server.abort();
}

#[test]
fn endpoints_and_provider_mappings_fail_closed() {
    let mut cfg = config("https://buzz.voltage.cloud");
    cfg.validate().unwrap();
    cfg.relay_url = "http://public.example".into();
    assert!(cfg.validate().is_err());
    cfg.relay_url = "https://buzz.voltage.cloud".into();
    // An absent Linear team, project, or webhook ID must not match a mapping.
    for field in 0..3 {
        let mut empty_key = cfg.clone();
        match field {
            0 => &mut empty_key.linear_teams,
            1 => &mut empty_key.linear_projects,
            _ => &mut empty_key.linear_webhooks,
        }
        .insert(String::new(), CHANNEL.parse().unwrap());
        assert!(empty_key.validate().is_err());
    }
    // Linear route records need a channel of their own, apart from every
    // mapped channel, so that no mapped membership can hide them.
    let routes: uuid::Uuid = ROUTES.parse().unwrap();
    for field in 0..5 {
        let mut overlap = cfg.clone();
        match field {
            0 => overlap
                .github_repos
                .insert("voltagecloud/other".into(), routes),
            1 => overlap.linear_teams.insert("ROUTES".into(), routes),
            2 => overlap.linear_projects.insert("ROUTES".into(), routes),
            3 => overlap.linear_webhooks.insert("ROUTES".into(), routes),
            _ => overlap.slack_channels.insert("T123/C999".into(), routes),
        };
        assert!(overlap.validate().is_err(), "mapping {field}");
    }
    let mut unrouted = cfg.clone();
    unrouted.linear_route_channel = None;
    assert!(unrouted.validate().is_err());
    unrouted.linear_teams.clear();
    unrouted.linear_webhooks.clear();
    unrouted.validate().unwrap();
    for field in 0..3 {
        let mut mapped = unrouted.clone();
        match field {
            0 => &mut mapped.linear_teams,
            1 => &mut mapped.linear_projects,
            _ => &mut mapped.linear_webhooks,
        }
        .insert("TEAM".into(), CHANNEL.parse().unwrap());
        assert!(mapped.validate().is_err(), "Linear mapping {field}");
    }
    cfg.slack_channels
        .insert("T123/GPRIVATE".into(), CHANNEL.parse().unwrap());
    assert!(cfg.validate().is_err());
}

/// A bounded burst of authenticated CI callbacks waits for the serialized
/// journal writer instead of rejecting every overlapping GitHub delivery.
#[tokio::test]
async fn authenticated_ci_bursts_preserve_every_delivery() {
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let router = app(config(&url), &keys);
    // Unsigned traffic must not use up the provider's per-second budget.
    for _ in 0..25 {
        assert_eq!(
            request(
                &router,
                "github",
                &github(),
                "unsigned",
                true,
                0,
                "pull_request"
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    // Replays of a captured, signed but stale Linear body are refused without
    // using up the Linear budget.
    let now_ms = nostr::Timestamp::now().as_secs() * 1000;
    let mut linear_issue = json!({"type":"Issue","action":"create","webhookTimestamp":now_ms - 600_000,"url":"https://linear.app/issue/ENG-1","data":{"id":"budget-issue","identifier":"ENG-1","teamId":"TEAM","title":"Budget","updatedAt":"2026-11-17T10:00:00.000Z"}});
    for _ in 0..25 {
        assert_eq!(
            request(
                &router,
                "linear",
                &linear_issue,
                "stale-replay",
                false,
                0,
                ""
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    linear_issue["webhookTimestamp"] = json!(now_ms);
    assert_eq!(
        request(
            &router,
            "linear",
            &linear_issue,
            "fresh-issue",
            false,
            0,
            ""
        )
        .await
        .0,
        StatusCode::OK
    );
    // A stalled unsigned upload releases its admission slot after one second.
    let stalled = axum::http::Request::builder()
        .method("POST")
        .uri("/ingest/github")
        .body(axum::body::Body::from_stream(
            futures_util::stream::pending::<Result<Bytes, std::io::Error>>(),
        ))
        .unwrap();
    let started = std::time::Instant::now();
    assert_eq!(
        router.clone().oneshot(stalled).await.unwrap().status(),
        StatusCode::REQUEST_TIMEOUT
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..5 {
        let router = router.clone();
        tasks.spawn(async move {
            let mut payload = github();
            payload["pull_request"]["number"] = json!(100 + index);
            request(
                &router,
                "github",
                &payload,
                &format!("burst-{index}"),
                false,
                0,
                "pull_request",
            )
            .await
            .0
        });
    }
    while let Some(outcome) = tasks.join_next().await {
        assert_eq!(outcome.unwrap(), StatusCode::OK);
    }
    assert_eq!(
        messages(&store)
            .iter()
            .filter(|event| tag(event, "ext-source") == Some("github"))
            .count(),
        10
    );
    server.abort();
}

const HOOKED: &str = "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a11";

/// A saved route is a fallback; current explicit signed webhook configuration
/// still decides where mapped activity goes. Only Issue versions move the
/// route, so comments without routing metadata follow the issue.
#[tokio::test]
async fn linear_current_mapping_overrides_saved_issue_route() {
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let router = app(config(&url), &keys);
    let now = nostr::Timestamp::now().as_secs() * 1000;
    let issue = json!({"type":"Issue","action":"create","webhookTimestamp":now,"data":{"id":"redirect-issue","identifier":"ENG-9","teamId":"TEAM","title":"Original route","updatedAt":"2026-11-17T10:00:00.000Z"},"url":"https://linear.app/issue/ENG-9"});
    let version = |team: &str, hook: bool, minute: u8, title: &str| {
        let mut payload = issue.clone();
        payload["action"] = json!("update");
        payload["data"]["teamId"] = json!(team);
        payload["data"]["title"] = json!(title);
        payload["data"]["updatedAt"] = json!(format!("2026-11-17T10:{minute:02}:00.000Z"));
        if hook {
            payload["webhookId"] = json!("HOOK");
        }
        payload
    };
    let channel_of = |body: &str| {
        messages(&store)
            .into_iter()
            .find(|event| event.content.contains(body))
            .and_then(|event| tag(&event, "h").map(str::to_owned))
    };
    let unmapped = |id: &str, body: &str| json!({"type":"Comment","action":"create","webhookTimestamp":now,"data":{"id":id,"issueId":"redirect-issue","body":body},"url":"https://linear.app/issue/ENG-9#comment"});
    assert_eq!(
        linear(&router, &issue, "route-create").await,
        StatusCode::OK
    );
    assert_eq!(
        linear(
            &router,
            &unmapped("unmapped-1", "Follows the team"),
            "route-unmapped-1"
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(channel_of("Follows the team").as_deref(), Some(CHANNEL));
    // A comment versions only itself, not the issue. An explicitly mapped one
    // goes to its webhook channel and leaves the issue route in place.
    let mut hooked = unmapped("hooked-comment", "Current hook");
    hooked["webhookId"] = json!("HOOK");
    assert_eq!(
        linear(&router, &hooked, "route-hooked").await,
        StatusCode::OK
    );
    assert_eq!(channel_of("Current hook").as_deref(), Some(HOOKED));
    assert_eq!(
        linear(
            &router,
            &unmapped("unmapped-2", "Still with the team"),
            "route-unmapped-2"
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(channel_of("Still with the team").as_deref(), Some(CHANNEL));
    // The issue moves to an unmapped team that the signed webhook ID maps.
    let moved = version("OTHER", true, 1, "Moved under the hook");
    assert_eq!(linear(&router, &moved, "route-move").await, StatusCode::OK);
    assert_eq!(
        linear(
            &router,
            &unmapped("unmapped-3", "Follows the move"),
            "route-unmapped-3"
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(channel_of("Follows the move").as_deref(), Some(HOOKED));
    // A route head signed later by write time, as by a clock running ahead,
    // must not keep a newer issue version from replacing it when the issue
    // returns to its team.
    {
        let mut store = store.lock().unwrap();
        let coordinate = store
            .heads
            .keys()
            .find(|d| d.starts_with("route:"))
            .unwrap()
            .clone();
        let current = store.events[&store.heads[&coordinate]].clone();
        let ahead = nostr::EventBuilder::new(current.kind, current.content.clone())
            .tags(current.tags.clone().to_vec())
            .custom_created_at(nostr::Timestamp::from(
                nostr::Timestamp::now().as_secs() + 60,
            ))
            .sign_with_keys(&keys)
            .unwrap();
        store.heads.insert(coordinate, ahead.id.to_hex());
        store.events.insert(ahead.id.to_hex(), ahead);
    }
    let returned = version("TEAM", false, 2, "Back with the team");
    assert_eq!(
        linear(&router, &returned, "route-return").await,
        StatusCode::OK
    );
    assert_eq!(
        linear(
            &router,
            &unmapped("unmapped-4", "Follows the return"),
            "route-unmapped-4"
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(channel_of("Follows the return").as_deref(), Some(CHANNEL));
    // A replay of the published move under a new delivery ID adds no message
    // and must not move the route back from the later return.
    let published = messages(&store).len();
    assert_eq!(
        linear(&router, &moved, "route-replay").await,
        StatusCode::OK
    );
    assert_eq!(messages(&store).len(), published);
    assert_eq!(
        linear(
            &router,
            &unmapped("unmapped-5", "Stays after the replay"),
            "route-unmapped-5"
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        channel_of("Stays after the replay").as_deref(),
        Some(CHANNEL)
    );
    // An unmapped comment that arrives while a move holds the plan writer
    // must follow that move, not the route it replaces.
    let redirect = version("OTHER", true, 3, "Overlapping move");
    let overlapping = unmapped("overlap-comment", "Follows the overlapping move");
    let queries = {
        let mut store = store.lock().unwrap();
        // Route reads stay fast, so an unserialized comment would read the
        // route before the move's slow write replaced it.
        store.submit_delay_ms = 150;
        store.queries
    };
    let send = |payload: Value, id: &'static str| {
        let router = router.clone();
        tokio::spawn(async move { linear(&router, &payload, id).await })
    };
    let redirecting = send(redirect, "route-overlap-move");
    // The move reads the relay only after it holds the plan writer.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.lock().unwrap().queries == queries {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let following = send(overlapping, "route-overlap-comment");
    assert_eq!(redirecting.await.unwrap(), StatusCode::OK);
    assert_eq!(following.await.unwrap(), StatusCode::OK);
    store.lock().unwrap().submit_delay_ms = 0;
    assert_eq!(
        channel_of("Follows the overlapping move").as_deref(),
        Some(HOOKED)
    );
    // The issue leaves every mapped scope. Its later comments without routing
    // metadata are acknowledged and ignored; they reach no channel.
    let left = version("OTHER", false, 4, "Left every mapped scope");
    assert_eq!(
        request(&router, "linear", &left, "route-leave", false, 0, "")
            .await
            .1["ignored"],
        true
    );
    let response = request(
        &router,
        "linear",
        &unmapped("unmapped-6", "Has no route"),
        "route-unmapped-6",
        false,
        0,
        "",
    )
    .await;
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.1["ignored"], true);
    assert_eq!(channel_of("Has no route"), None);
    server.abort();
}

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    No,
    LoseAnchor,
    Route(RecordFault),
}

/// Linear delivers and retries issue versions in any order. Each row is one
/// issue's delivery order. A later comment without routing metadata must
/// follow the newest version, wherever it fell in that order, or be ignored
/// when that version left every mapped scope. `team` maps by team, `hook` by
/// signed webhook ID, `gone` names an unmapped team, and `partial` maps by
/// webhook ID without any team or project field.
#[tokio::test]
async fn linear_issue_routes_converge_on_the_newest_version() {
    use Fault::*;
    type Step = (&'static str, &'static str, Fault, StatusCode);
    const OK: StatusCode = StatusCode::OK;
    const RETRY: StatusCode = StatusCode::SERVICE_UNAVAILABLE;
    const REFUSED: StatusCode = StatusCode::BAD_REQUEST;
    let rows: &[(&str, &[Step], Option<&str>)] = &[
        (
            "late deliveries of a move away clear the route",
            &[("team", "01.000", No, OK), ("gone", "02.000", No, OK)],
            None,
        ),
        (
            "a move away delivered first outranks the older mapped version",
            &[("gone", "02.000", No, OK), ("team", "01.000", No, OK)],
            None,
        ),
        (
            "a newer mapped version restores a cleared route",
            &[("gone", "02.000", No, OK), ("team", "03.000", No, OK)],
            Some(CHANNEL),
        ),
        (
            "a replayed move away cannot clear the route of a newer return",
            &[
                ("team", "01.000", No, OK),
                ("gone", "02.000", No, OK),
                ("team", "03.000", No, OK),
                ("gone", "02.000", No, OK),
            ],
            Some(CHANNEL),
        ),
        (
            "a retried mapped version cannot restore the route after a newer move away",
            &[
                ("team", "01.000", No, OK),
                ("hook", "01.500", LoseAnchor, RETRY),
                ("gone", "02.000", No, OK),
                ("hook", "01.500", No, OK),
            ],
            None,
        ),
        (
            "versions within one second compare at full precision",
            &[("team", "05.100", No, OK), ("gone", "05.200", No, OK)],
            None,
        ),
        (
            "a newer clear advances the version that a mapped version must beat",
            &[
                ("team", "01.000", No, OK),
                ("gone", "02.000", No, OK),
                ("gone", "04.000", No, OK),
                ("team", "03.000", No, OK),
            ],
            None,
        ),
        (
            "a newer version in the same channel advances the version",
            &[
                ("team", "01.000", No, OK),
                ("team", "03.000", No, OK),
                ("gone", "02.000", No, OK),
            ],
            Some(CHANNEL),
        ),
        (
            "for one version, a channel outranks a clear delivered after it",
            &[("hook", "06.000", No, OK), ("gone", "06.000", No, OK)],
            Some(HOOKED),
        ),
        (
            "for one version, a channel outranks a clear delivered before it",
            &[("gone", "06.000", No, OK), ("hook", "06.000", No, OK)],
            Some(HOOKED),
        ),
        (
            "a mapped version without team or project fields still moves the route",
            &[("team", "01.000", No, OK), ("partial", "02.000", No, OK)],
            Some(HOOKED),
        ),
        (
            "a missing or invalid version is refused and changes nothing",
            &[
                ("team", "01.000", No, OK),
                ("gone", "", No, REFUSED),
                ("gone", "1", No, REFUSED),
                ("team", "", No, REFUSED),
            ],
            Some(CHANNEL),
        ),
        (
            "a failed route write is not acknowledged, and its retry completes",
            &[
                ("team", "01.000", No, OK),
                ("gone", "02.000", Route(RecordFault::Refuse), RETRY),
                ("gone", "02.000", Route(RecordFault::Outrun), RETRY),
                ("gone", "02.000", Route(RecordFault::LoseAck), RETRY),
                ("gone", "02.000", No, OK),
            ],
            None,
        ),
    ];
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let now = nostr::Timestamp::now().as_secs() * 1000;
    for (row, (name, steps, expected)) in rows.iter().enumerate() {
        // One publisher and relay; a fresh process keeps each row within the
        // provider's per-second delivery budget.
        let router = app(config(&url), &keys);
        let issue = format!("ordered-issue-{row}");
        for (variant, at, fault, status) in steps.iter() {
            let mut payload = json!({"type":"Issue","action":"update","webhookTimestamp":now,"url":format!("https://linear.app/issue/{issue}"),"data":{"id":issue,"identifier":issue,"teamId":if *variant == "team" {"TEAM"} else {"OTHER"},"title":format!("{variant} at {at}")}});
            if *variant == "hook" || *variant == "partial" {
                payload["webhookId"] = json!("HOOK");
            }
            if *variant == "partial" {
                payload["data"].as_object_mut().unwrap().remove("teamId");
            }
            if !at.is_empty() {
                payload["data"]["updatedAt"] = json!(format!("2026-11-17T10:00:{at}Z"));
            }
            {
                let mut store = store.lock().unwrap();
                store.lose_root_response = *fault == LoseAnchor;
                if let Route(fault) = fault {
                    store.record_fault = Some(("route:", *fault));
                }
            }
            // Identical payloads share a delivery ID, so a repeat is a retry.
            let delivery = format!("{issue}-{variant}-{}", at.replace('.', "-"));
            assert_eq!(
                linear(&router, &payload, &delivery).await,
                *status,
                "{name}"
            );
            let store = store.lock().unwrap();
            assert!(
                !store.lose_root_response && store.record_fault.is_none(),
                "{name}: the fault must reach its write"
            );
        }
        let body = format!("Comment on {issue}");
        let comment = json!({"type":"Comment","action":"create","webhookTimestamp":now,"url":format!("https://linear.app/issue/{issue}#comment"),"data":{"id":format!("{issue}-comment"),"issueId":issue,"body":body}});
        let (status, response) = request(
            &router,
            "linear",
            &comment,
            &format!("{issue}-comment"),
            false,
            0,
            "",
        )
        .await;
        let channel = messages(&store)
            .into_iter()
            .find(|event| event.content.contains(&body))
            .and_then(|event| tag(&event, "h").map(str::to_owned));
        assert_eq!(
            (status, response["ignored"].as_bool(), channel.as_deref()),
            (OK, Some(expected.is_none()), *expected),
            "{name}"
        );
    }
    server.abort();
}

/// Linear can deliver an issue's move out of every mapped scope late, or retry
/// it after a newer mapped update. Neither may retire the newer route, across
/// failed route writes and restarts. An acknowledged move is durable first.
#[tokio::test]
async fn linear_delayed_removals_cannot_retire_a_newer_route() {
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let cfg = config(&url);
    let now = nostr::Timestamp::now().as_secs() * 1000;
    let issue = |id: &str, team: &str, second: u8| json!({"type":"Issue","action":"update","webhookTimestamp":now,"data":{"id":id,"identifier":id,"teamId":team,"title":format!("{team} at {second}"),"updatedAt":format!("2026-11-17T10:00:{second:02}.000Z")},"url":format!("https://linear.app/issue/{id}")});
    let comment = |issue: &str, body: &str| json!({"type":"Comment","action":"create","webhookTimestamp":now,"data":{"id":body,"issueId":issue,"body":body},"url":format!("https://linear.app/issue/{issue}#comment")});
    let send = |router: Router, payload: Value, id: String| async move {
        request(&router, "linear", &payload, &id, false, 0, "").await
    };
    // A comment's outcome: ignored, or the channel that received it.
    let route = |router: Router, issue: &'static str, body: &'static str| {
        let store = store.clone();
        let sent = send(router, comment(issue, body), body.replace(' ', "-"));
        async move {
            let (status, response) = sent.await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let channel = messages(&store)
                .into_iter()
                .find(|event| event.content.contains(body))
                .and_then(|event| tag(&event, "h").map(str::to_owned));
            assert_eq!(response["ignored"] == true, channel.is_none(), "{body}");
            channel
        }
    };
    let first = app(cfg.clone(), &keys);
    // A never-mapped issue: its first delivered version already left every
    // mapped scope. Saving that decision stops its retry from retiring the
    // route of the newer mapped version that follows.
    let fresh_removal = issue("fresh-issue", "UNMAPPED", 2);
    let (status, response) =
        send(first.clone(), fresh_removal.clone(), "fresh-removal".into()).await;
    assert_eq!(
        (status, &response["ignored"]),
        (StatusCode::OK, &json!(true))
    );
    assert_eq!(
        route(first.clone(), "fresh-issue", "Fresh unmapped").await,
        None
    );
    let mapped = issue("fresh-issue", "TEAM", 3);
    assert_eq!(
        send(first.clone(), mapped, "fresh-mapped".into()).await.0,
        StatusCode::OK
    );
    // A mapped issue, with a partial update that carries no scope fields and
    // no version. It decides nothing, so the route stays.
    assert_eq!(
        send(
            first.clone(),
            issue("moved-issue", "TEAM", 1),
            "moved-1".into()
        )
        .await
        .0,
        StatusCode::OK
    );
    let partial = json!({"type":"Issue","action":"update","webhookTimestamp":now,"data":{"id":"moved-issue","title":"No scope information"},"url":"https://linear.app/issue/moved-issue"});
    assert_eq!(
        send(first.clone(), partial, "partial".into()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        route(first.clone(), "moved-issue", "After a partial update")
            .await
            .as_deref(),
        Some(CHANNEL)
    );
    // Its move away fails before the route changes: first the route read,
    // then the route write. A newer mapped version then succeeds.
    let removal = issue("moved-issue", "UNMAPPED", 2);
    store.lock().unwrap().fail_queries = 1;
    assert_eq!(
        send(first.clone(), removal.clone(), "moved-2".into())
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    store.lock().unwrap().record_fault = Some(("route:", RecordFault::Refuse));
    assert_eq!(
        send(first.clone(), removal.clone(), "moved-2".into())
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(store.lock().unwrap().record_fault.is_none());
    assert_eq!(
        send(
            first.clone(),
            issue("moved-issue", "TEAM", 3),
            "moved-3".into()
        )
        .await
        .0,
        StatusCode::OK
    );
    // After a restart, both delayed removals are retried. Each route stays
    // with the newer mapped version.
    let restarted = app(cfg.clone(), &keys);
    for (payload, id) in [(fresh_removal, "fresh-removal"), (removal, "moved-2")] {
        assert_eq!(
            send(restarted.clone(), payload, id.into()).await.0,
            StatusCode::OK
        );
    }
    assert_eq!(
        route(restarted.clone(), "fresh-issue", "Fresh after retry")
            .await
            .as_deref(),
        Some(CHANNEL)
    );
    assert_eq!(
        route(restarted.clone(), "moved-issue", "Moved after retry")
            .await
            .as_deref(),
        Some(CHANNEL)
    );
    // A newer move away is saved, but its acknowledgement is lost. The move
    // is already durable: after a restart, comments no longer reach the old
    // channel, and the retry completes.
    let published = messages(&store).len();
    let newest_removal = issue("moved-issue", "UNMAPPED", 4);
    store.lock().unwrap().record_fault = Some(("route:", RecordFault::LoseAck));
    assert_eq!(
        send(restarted.clone(), newest_removal.clone(), "moved-4".into())
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(store.lock().unwrap().record_fault.is_none());
    let third = app(cfg.clone(), &keys);
    assert_eq!(
        route(third.clone(), "moved-issue", "Hidden after loss").await,
        None
    );
    let writes = store.lock().unwrap().submissions.len();
    assert_eq!(
        send(third.clone(), newest_removal.clone(), "moved-4".into())
            .await
            .0,
        StatusCode::OK
    );
    // The retry finds its decision already saved and writes nothing.
    assert_eq!(store.lock().unwrap().submissions.len(), writes);
    assert_eq!(messages(&store).len(), published);
    // The issue returns. A replay of the older move under a new delivery ID
    // cannot retire the restored route.
    assert_eq!(
        send(
            third.clone(),
            issue("moved-issue", "TEAM", 5),
            "moved-5".into()
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        send(third.clone(), newest_removal, "moved-4-replay".into())
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        route(third.clone(), "moved-issue", "Back in the mapped channel")
            .await
            .as_deref(),
        Some(CHANNEL)
    );
    // A configuration that no longer maps the saved channel refuses the
    // comment for retry instead of following the route there.
    let mut remapped = cfg.clone();
    remapped
        .linear_teams
        .insert("TEAM".into(), HOOKED.parse().unwrap());
    let (status, _) = send(
        app(remapped, &keys),
        comment("moved-issue", "Outside the configuration"),
        "outside-configuration".into(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!messages(&store)
        .iter()
        .any(|event| event.content.contains("Outside the configuration")));
    // A route write with an unknown outcome can reach the relay after a newer
    // one, also after the publisher restarted and forgot it. The newer route
    // stays in either landing order.
    let restart = || app(cfg.clone(), &keys);
    let mut hooked = issue("moved-issue", "UNMAPPED", 6);
    hooked["webhookId"] = json!("HOOK");
    store.lock().unwrap().record_fault = Some(("route:", RecordFault::Hold));
    assert_eq!(
        send(restart(), hooked, "moved-6".into()).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        send(
            restart(),
            issue("moved-issue", "UNMAPPED", 7),
            "moved-7".into()
        )
        .await
        .0,
        StatusCode::OK
    );
    land(&store);
    assert_eq!(
        route(restart(), "moved-issue", "Hidden after a late write").await,
        None
    );
    // A newer move away has an unknown outcome when, after a restart, an older
    // return arrives. The return replaces the saved route until the move lands.
    store.lock().unwrap().record_fault = Some(("route:", RecordFault::Hold));
    assert_eq!(
        send(
            restart(),
            issue("moved-issue", "UNMAPPED", 9),
            "moved-9".into()
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        send(restart(), issue("moved-issue", "TEAM", 8), "moved-8".into())
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        route(restart(), "moved-issue", "Follows the older return")
            .await
            .as_deref(),
        Some(CHANNEL)
    );
    land(&store);
    assert_eq!(
        route(
            restart(),
            "moved-issue",
            "Hidden after the newer move lands"
        )
        .await,
        None
    );
    // A saved route without a valid version, as a relay without route
    // validation could keep, refuses the comment for retry.
    let coordinate = format!(
        "route:linear:{}",
        hex::encode(Sha256::digest("moved-issue"))
    );
    {
        let mut store = store.lock().unwrap();
        let current = store.events[&store.heads[&coordinate]].clone();
        let mut tags = current.tags.clone().to_vec();
        tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("updated"));
        tags.push(nostr::Tag::parse(["updated", "unknown"]).unwrap());
        let invalid = nostr::EventBuilder::new(current.kind, current.content.clone())
            .tags(tags)
            .sign_with_keys(&keys)
            .unwrap();
        store.heads.insert(coordinate, invalid.id.to_hex());
        store.events.insert(invalid.id.to_hex(), invalid);
    }
    let (status, _) = send(
        restart(),
        comment("moved-issue", "Behind an invalid route"),
        "invalid-route".into(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    server.abort();
}

/// Releases before version ordering saved an issue's route as a journal plan
/// at `route:linear:issue:<id>`, without an issue version: the thread anchor
/// in its mapped channel, re-signed with `ext-route-state=unmapped` after the
/// issue left every mapped scope. That state routes comments until the issue's
/// first versioned route. A released write cannot undo the versioned route.
#[tokio::test]
async fn linear_released_routes_hold_until_the_first_issue_version() {
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let router = app(config(&url), &keys);
    let now = nostr::Timestamp::now().as_secs();
    // Save a route record in the released format, signed at `created_at`.
    let released = |issue: &str, retired: bool, created_at: u64| {
        let tag = |name: &str, value: &str| nostr::Tag::parse([name, value]).unwrap();
        let mut anchor = vec![
            tag("h", CHANNEL),
            tag("ext-thread", &format!("issue:{issue}")),
            tag("ext-source", "linear"),
        ];
        if retired {
            anchor.push(tag("ext-route-state", "unmapped"));
        }
        let content = format!("linear: {issue}\nhttps://linear.app/issue/{issue}");
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), content)
            .tags(anchor.clone())
            .sign_with_keys(&keys)
            .unwrap();
        let plan =
            json!({"version":1,"payload_sha256":retired.then(|| "ab".repeat(32)),"event":event});
        let mut tags = vec![
            tag("d", &format!("route:linear:issue:{issue}")),
            tag("h", CHANNEL),
            tag("message", &event.id.to_hex()),
        ];
        tags.extend(anchor.into_iter().skip(1));
        let kind = nostr::Kind::Custom(buzz_core::kind::KIND_INTEGRATION_RECORD as u16);
        let record = nostr::EventBuilder::new(kind, STANDARD.encode(plan.to_string()))
            .tags(tags)
            .custom_created_at(nostr::Timestamp::from(created_at))
            .sign_with_keys(&keys)
            .unwrap();
        assert!(commit(&mut store.lock().unwrap(), &record));
    };
    let issue = |id: &str, hook: bool| {
        let mut payload = json!({"type":"Issue","action":"update","webhookTimestamp":now * 1000,"data":{"id":id,"identifier":id,"teamId":"OTHER","title":id,"updatedAt":"2026-11-17T09:00:00.000Z"},"url":format!("https://linear.app/issue/{id}")});
        if hook {
            payload["webhookId"] = json!("HOOK");
        }
        payload
    };
    let comment = |issue: &str, body: &str| json!({"type":"Comment","action":"create","webhookTimestamp":now * 1000,"data":{"id":body,"issueId":issue,"body":body},"url":format!("https://linear.app/issue/{issue}#comment")});
    // A comment's outcome: ignored, or the channel that received it.
    let route = |issue: &'static str, body: &'static str| {
        let (router, store) = (router.clone(), store.clone());
        async move {
            let (status, response) = request(
                &router,
                "linear",
                &comment(issue, body),
                &body.replace(' ', "-"),
                false,
                0,
                "",
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let channel = messages(&store)
                .into_iter()
                .find(|event| event.content.contains(body))
                .and_then(|event| tag(&event, "h").map(str::to_owned));
            assert_eq!(response["ignored"] == true, channel.is_none(), "{body}");
            channel
        }
    };
    released("mapped-issue", false, now);
    released("retired-issue", true, now);
    assert_eq!(
        route("mapped-issue", "Follows the released route")
            .await
            .as_deref(),
        Some(CHANNEL)
    );
    assert_eq!(
        route("retired-issue", "Ignored after the released retirement").await,
        None
    );
    // While the publisher cannot read the route channel, a versioned route may
    // be hidden, so the released route is not trusted: comments wait for retry.
    // A released route hidden by a removed membership is unknown, so comments
    // are refused as if the issue were never delivered.
    for (removed, body, status) in [
        (
            ROUTES,
            "Waits for the route channel",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (CHANNEL, "Released route is hidden", StatusCode::BAD_REQUEST),
    ] {
        store.lock().unwrap().removed.insert(removed.into());
        let sent = request(
            &router,
            "linear",
            &comment("mapped-issue", body),
            &body.replace(' ', "-"),
            false,
            0,
            "",
        )
        .await;
        store.lock().unwrap().removed.clear();
        assert_eq!(sent.0, status, "{body}");
        assert!(!messages(&store)
            .iter()
            .any(|event| event.content.contains(body)));
    }
    // The first versioned decision replaces the released state. A released
    // record has no issue version to compare, so even a version older than
    // its write replaces it.
    for (payload, id) in [
        (issue("mapped-issue", false), "mapped-leaves"),
        (issue("retired-issue", true), "retired-returns"),
    ] {
        assert_eq!(linear(&router, &payload, id).await, StatusCode::OK);
    }
    // Released writes that land later by relay time change nothing.
    released("mapped-issue", false, now + 60);
    released("retired-issue", true, now + 60);
    assert_eq!(
        route("mapped-issue", "Ignored after the versioned clear").await,
        None
    );
    assert_eq!(
        route("retired-issue", "Follows the versioned return")
            .await
            .as_deref(),
        Some(HOOKED)
    );
    // The route channel membership returns right after a read that it hid.
    // The released route must not be trusted on that read: the versioned
    // clear, readable again, still wins.
    {
        let mut store = store.lock().unwrap();
        store.removed.insert(ROUTES.into());
        store.restore_after_query = Some(ROUTES.into());
    }
    assert_eq!(
        route("mapped-issue", "Ignored after membership returns").await,
        None
    );
    assert!(store.lock().unwrap().restore_after_query.is_none());
    server.abort();
}

/// Route records live in the publisher-only route channel. A removed
/// membership in a mapped channel therefore hides no route: an older version
/// cannot replace it, and its comments wait for that channel instead of going
/// elsewhere. Without the route channel itself, routes wait for retry.
#[tokio::test]
async fn linear_routes_survive_loss_of_destination_membership() {
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let router = app(config(&url), &keys);
    let now = nostr::Timestamp::now().as_secs() * 1000;
    let issue = |team: &str, hook: bool, second: u8| {
        let mut payload = json!({"type":"Issue","action":"update","webhookTimestamp":now,"data":{"id":"kept-issue","identifier":"ENG-7","teamId":team,"title":format!("{team} at {second}"),"updatedAt":format!("2026-11-17T10:00:{second:02}.000Z")},"url":"https://linear.app/issue/ENG-7"});
        if hook {
            payload["webhookId"] = json!("HOOK");
        }
        payload
    };
    let send = |payload: Value, id: &str| {
        let router = router.clone();
        let id = id.to_owned();
        async move { request(&router, "linear", &payload, &id, false, 0, "").await }
    };
    // A comment's status and the channel that received it, if any.
    let comment = |body: &'static str| {
        let (store, sent) = (
            store.clone(),
            send(
                json!({"type":"Comment","action":"create","webhookTimestamp":now,"data":{"id":body,"issueId":"kept-issue","body":body},"url":"https://linear.app/issue/ENG-7#comment"}),
                &body.replace(' ', "-"),
            ),
        );
        async move {
            let (status, _) = sent.await;
            let channel = messages(&store)
                .into_iter()
                .find(|event| event.content.contains(body))
                .and_then(|event| tag(&event, "h").map(str::to_owned));
            (status, channel)
        }
    };
    let removed = |channel: &str, gone: bool| {
        let mut store = store.lock().unwrap();
        if gone {
            store.removed.insert(channel.into());
        } else {
            store.removed.remove(channel);
        }
    };
    assert_eq!(
        send(issue("TEAM", false, 2), "kept-2").await.0,
        StatusCode::OK
    );
    let coordinate = format!("route:linear:{}", hex::encode(Sha256::digest("kept-issue")));
    {
        let store = store.lock().unwrap();
        let record = &store.events[&store.heads[&coordinate]];
        assert_eq!(tag(record, "h"), Some(ROUTES));
        assert_eq!(tag(record, "route"), Some(CHANNEL));
    }
    // The publisher leaves the mapped channel. An older version mapped by the
    // webhook still publishes to its own channel, but keeps the newer route.
    removed(CHANNEL, true);
    assert_eq!(
        send(issue("OTHER", true, 1), "kept-1").await.0,
        StatusCode::OK
    );
    assert_eq!(
        comment("Waits for its channel").await,
        (StatusCode::SERVICE_UNAVAILABLE, None)
    );
    removed(CHANNEL, false);
    assert_eq!(
        comment("Waits for its channel").await,
        (StatusCode::OK, Some(CHANNEL.to_owned()))
    );
    // A newer move away clears the route while the membership is gone.
    removed(CHANNEL, true);
    let (status, response) = send(issue("OTHER", false, 3), "kept-3").await;
    assert_eq!(
        (status, &response["ignored"]),
        (StatusCode::OK, &json!(true))
    );
    assert_eq!(comment("Has no route").await, (StatusCode::OK, None));
    removed(CHANNEL, false);
    // Without the route channel, a route can be neither saved nor read.
    removed(ROUTES, true);
    assert_eq!(
        send(issue("TEAM", false, 4), "kept-4").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        comment("Waits for the route channel").await,
        (StatusCode::SERVICE_UNAVAILABLE, None)
    );
    removed(ROUTES, false);
    assert_eq!(
        send(issue("TEAM", false, 4), "kept-4").await.0,
        StatusCode::OK
    );
    assert_eq!(
        comment("Waits for the route channel").await,
        (StatusCode::OK, Some(CHANNEL.to_owned()))
    );
    server.abort();
}

/// Recovery can replace an old missing root while its unsent reply is still
/// inside the timestamp window. Both NIP-10 references must follow the new root.
#[tokio::test]
async fn github_recovery_rebinds_a_fresh_reply_to_its_recreated_anchor() {
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let router = app(config(&url), &keys);
    let payload = github();
    store.lock().unwrap().fail_reply = 1;
    assert_eq!(
        request(
            &router,
            "github",
            &payload,
            "fresh-reply",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let root = messages(&store)
        .into_iter()
        .find(|e| tag(e, "e").is_none())
        .unwrap();
    let aged = nostr::EventBuilder::new(root.kind, root.content.clone())
        .tags(root.tags.to_vec())
        .custom_created_at(nostr::Timestamp::from(
            nostr::Timestamp::now().as_secs() - 1000,
        ))
        .sign_with_keys(&keys)
        .unwrap();
    let fresh_time;
    {
        let mut locked = store.lock().unwrap();
        locked.events.remove(&root.id.to_hex());
        let records: Vec<_> = locked
            .heads
            .values()
            .map(|id| locked.events[id].clone())
            .collect();
        fresh_time = records
            .iter()
            .find_map(|record| {
                let plan: Value =
                    serde_json::from_slice(&STANDARD.decode(&record.content).unwrap()).unwrap();
                let saved: Event = serde_json::from_value(plan["event"].clone()).unwrap();
                tag(&saved, "e").map(|_| saved.created_at)
            })
            .unwrap();
        assert!(
            nostr::Timestamp::now()
                .as_secs()
                .abs_diff(fresh_time.as_secs())
                < 10
        );
        for record in records {
            let mut plan: Value =
                serde_json::from_slice(&STANDARD.decode(&record.content).unwrap()).unwrap();
            let saved: Event = serde_json::from_value(plan["event"].clone()).unwrap();
            let replacement = if tag(&saved, "e").is_none() {
                aged.clone()
            } else {
                let mut tags = saved.tags.to_vec();
                tags.retain(|tag| tag.as_slice()[0] != "e");
                tags.push(nostr::Tag::parse(["e", &aged.id.to_hex(), "", "root"]).unwrap());
                tags.push(nostr::Tag::parse(["e", &aged.id.to_hex(), "", "reply"]).unwrap());
                nostr::EventBuilder::new(saved.kind, saved.content)
                    .tags(tags)
                    .custom_created_at(saved.created_at)
                    .sign_with_keys(&keys)
                    .unwrap()
            };
            plan["event"] = serde_json::to_value(&replacement).unwrap();
            let mut tags = record.tags.to_vec();
            tags.retain(|tag| tag.as_slice()[0] != "message");
            tags.push(nostr::Tag::parse(["message", &replacement.id.to_hex()]).unwrap());
            let rewritten = nostr::EventBuilder::new(
                record.kind,
                STANDARD.encode(serde_json::to_vec(&plan).unwrap()),
            )
            .tags(tags)
            .custom_created_at(record.created_at)
            .sign_with_keys(&keys)
            .unwrap();
            locked
                .heads
                .insert(tag(&rewritten, "d").unwrap().into(), rewritten.id.to_hex());
            locked.events.insert(rewritten.id.to_hex(), rewritten);
        }
    }
    assert_eq!(
        request(
            &router,
            "github",
            &payload,
            "fresh-reply",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::OK
    );
    let published = messages(&store);
    assert_eq!(published.len(), 2);
    let anchor = published.iter().find(|e| tag(e, "e").is_none()).unwrap();
    let reply = published.iter().find(|e| tag(e, "e").is_some()).unwrap();
    let (root_id, parent) = buzz_core::nip10::parse_thread_markers(&reply.tags)
        .resolve()
        .unwrap();
    assert_eq!(root_id, anchor.id.to_hex());
    assert_eq!(parent, root_id);
    assert_ne!(root_id, aged.id.to_hex());
    assert_eq!(
        reply.created_at, fresh_time,
        "fresh replies keep their frozen timestamp"
    );
    assert_eq!(
        request(
            &router,
            "github",
            &payload,
            "new-header",
            false,
            0,
            "pull_request"
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        messages(&store).len(),
        2,
        "recovery replay duplicated an accepted message"
    );
    server.abort();
}
