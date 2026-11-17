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
#[derive(Default)]
struct Store {
    events: BTreeMap<String, Event>,
    heads: BTreeMap<String, String>,
    fail_queries: usize,
    fail_reply: usize,
    fail_record_prefix: Option<String>,
    fail_event_lookup: Option<String>,
    lose_root_response: bool,
    submissions: Vec<Event>,
    queries: usize,
    delay_ms: u64,
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
    (StatusCode::OK, Json(json!(events)))
}
async fn submit(
    State(store): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    auth(&headers, &body);
    let delay = store.lock().unwrap().delay_ms;
    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    let event: Event = serde_json::from_slice(&body).unwrap();
    event.verify().unwrap();
    let mut store = store.lock().unwrap();
    store.submissions.push(event.clone());
    if event.kind.as_u16() == buzz_core::kind::KIND_INTEGRATION_RECORD as u16
        && store
            .fail_record_prefix
            .as_ref()
            .is_some_and(|prefix| tag(&event, "d").is_some_and(|d| d.starts_with(prefix)))
    {
        store.fail_record_prefix = None;
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
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
                return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
            }
        }
    }
    let id = event.id.to_hex();
    let duplicate = (
        StatusCode::OK,
        Json(json!({"event_id":id,"accepted":true,"message":"duplicate:"})),
    );
    if store.events.contains_key(&id) {
        return duplicate;
    }
    if let Some(d) = tag(&event, "d") {
        // Model the relay's addressable replacement: the newest record wins,
        // a same-second tie keeps the lowest ID, and a losing write is
        // acknowledged as a duplicate without being stored.
        let keeps_head = store
            .heads
            .get(d)
            .and_then(|head| store.events.get(head))
            .is_some_and(|head| {
                head.created_at > event.created_at
                    || (head.created_at == event.created_at && head.id.to_hex() < id)
            });
        if keeps_head {
            return duplicate;
        }
        store.heads.insert(d.to_owned(), id.clone());
    }
    store.events.insert(id.clone(), event.clone());
    if event.kind.as_u16() == 9 && tag(&event, "e").is_none() && store.lose_root_response {
        store.lose_root_response = false;
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    (
        StatusCode::OK,
        Json(json!({"event_id":id,"accepted":true,"message":""})),
    )
}
async fn external() -> (String, Shared, tokio::task::JoinHandle<()>) {
    let store = Shared::default();
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
    serde_json::from_value(json!({"relay_url":url,"github_repos":{"voltagecloud/buzz":CHANNEL},"linear_teams":{"TEAM":CHANNEL},"linear_webhooks":{"HOOK":"0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a11"},"linear_labels":{"LABEL-AGENT":"agent"},"slack_channels":{"T123/C123":CHANNEL},"slack_workspace_url":"https://voltage.slack.com"})).unwrap()
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
    let issue = json!({"type":"Issue","action":"create","webhookTimestamp":now,"webhookId":"HOOK","url":"https://linear.app/issue/ENG-7","actor":{"name":"Ada"},"data":{"id":"issue-7","identifier":"ENG-7","teamId":"TEAM","title":"Fix it","description":"Details","stateId":"STATE-TODO"}});
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
    let mut linear_issue = json!({"type":"Issue","action":"create","webhookTimestamp":now_ms - 600_000,"url":"https://linear.app/issue/ENG-1","data":{"id":"budget-issue","identifier":"ENG-1","teamId":"TEAM","title":"Budget"}});
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

/// A saved route is a fallback; current explicit signed webhook configuration
/// can redirect subsequent activity without leaking it to the previous channel.
#[tokio::test]
async fn linear_current_mapping_overrides_saved_issue_route() {
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let router = app(config(&url), &keys);
    let now = nostr::Timestamp::now().as_secs() * 1000;
    let issue = json!({"type":"Issue","action":"create","webhookTimestamp":now,"data":{"id":"redirect-issue","identifier":"ENG-9","teamId":"TEAM","title":"Original route"},"url":"https://linear.app/issue/ENG-9"});
    assert_eq!(
        request(&router, "linear", &issue, "route-before", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    let comment = json!({"type":"Comment","action":"create","webhookTimestamp":now,"webhookId":"HOOK","data":{"id":"redirect-comment","issueId":"redirect-issue","body":"Current route"},"url":"https://linear.app/issue/ENG-9#comment"});
    assert_eq!(
        request(&router, "linear", &comment, "route-after", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    let imported = messages(&store)
        .into_iter()
        .find(|event| event.content.contains("Current route"))
        .unwrap();
    assert_eq!(
        tag(&imported, "h"),
        Some("0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a11")
    );
    let channel_of = |body: &str| {
        messages(&store)
            .into_iter()
            .find(|event| event.content.contains(body))
            .and_then(|event| tag(&event, "h").map(str::to_owned))
    };
    let unmapped = |id: &str, body: &str| json!({"type":"Comment","action":"create","webhookTimestamp":now,"data":{"id":id,"issueId":"redirect-issue","body":body},"url":"https://linear.app/issue/ENG-9#comment"});
    // The issue returns to its team channel, where its anchor already exists.
    // Comments without routing metadata must follow it back.
    let mut returned = issue.clone();
    returned["action"] = json!("update");
    returned["data"]["title"] = json!("Back with the team");
    assert_eq!(
        request(&router, "linear", &returned, "route-return", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &router,
            "linear",
            &unmapped("unmapped-1", "Follows the team"),
            "route-unmapped-1",
            false,
            0,
            ""
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(channel_of("Follows the team").as_deref(), Some(CHANNEL));
    // The relay keeps a route head that is not older than this write, as in a
    // same-second tie, and acknowledges the write as a duplicate. The current
    // explicit route must still replace it.
    {
        let mut store = store.lock().unwrap();
        let coordinate = "route:linear:issue:redirect-issue";
        let current = store.events[&store.heads[coordinate]].clone();
        let ahead = nostr::EventBuilder::new(current.kind, current.content.clone())
            .tags(current.tags.clone().to_vec())
            .custom_created_at(nostr::Timestamp::from(
                nostr::Timestamp::now().as_secs() + 60,
            ))
            .sign_with_keys(&keys)
            .unwrap();
        store.heads.insert(coordinate.into(), ahead.id.to_hex());
        store.events.insert(ahead.id.to_hex(), ahead);
    }
    let mut hooked = comment.clone();
    hooked["data"]["id"] = json!("hooked-again");
    hooked["data"]["body"] = json!("Hooked again");
    assert_eq!(
        request(&router, "linear", &hooked, "route-hooked", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &router,
            "linear",
            &unmapped("unmapped-2", "Follows the hook"),
            "route-unmapped-2",
            false,
            0,
            ""
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        channel_of("Follows the hook").as_deref(),
        Some("0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a11")
    );
    // A replay of the published team update under a new delivery ID adds no
    // message and must not move the route back from the later redirect.
    let published = messages(&store).len();
    assert_eq!(
        request(&router, "linear", &returned, "route-replay", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(messages(&store).len(), published);
    assert_eq!(
        request(
            &router,
            "linear",
            &unmapped("unmapped-3", "Still follows the hook"),
            "route-unmapped-3",
            false,
            0,
            ""
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        channel_of("Still follows the hook").as_deref(),
        Some("0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a11")
    );
    // The issue returns to its team. An unmapped comment that arrives while a
    // redirect holds the plan writer must follow that redirect, not the team
    // route it replaces.
    let mut team_again = returned.clone();
    team_again["data"]["title"] = json!("Team again");
    assert_eq!(
        request(&router, "linear", &team_again, "route-team", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    let mut redirect = comment.clone();
    redirect["data"]["id"] = json!("overlap-redirect");
    redirect["data"]["body"] = json!("Overlapping redirect");
    let overlapping = unmapped("overlap-comment", "Follows the overlapping redirect");
    let queries = {
        let mut store = store.lock().unwrap();
        store.delay_ms = 80;
        store.queries
    };
    let send = |payload: Value, id: &'static str| {
        let router = router.clone();
        tokio::spawn(async move {
            request(&router, "linear", &payload, id, false, 0, "")
                .await
                .0
        })
    };
    let redirecting = send(redirect, "route-overlap-hook");
    // The redirect reads the relay only after it holds the plan writer.
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
    store.lock().unwrap().delay_ms = 0;
    assert_eq!(
        channel_of("Follows the overlapping redirect").as_deref(),
        Some("0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a11")
    );
    server.abort();
}

/// An acknowledged scope removal cannot keep sending comments to the old
/// channel, including after restart or a completed removal replay.
#[tokio::test]
async fn linear_unmapped_scope_retires_route_before_acknowledgement() {
    let (url, store, server) = external().await;
    let keys = Keys::generate();
    let cfg = config(&url);
    let first = app(cfg.clone(), &keys);
    let now = nostr::Timestamp::now().as_secs() * 1000;
    let issue = json!({"type":"Issue","action":"create","webhookTimestamp":now,"data":{"id":"retired-issue","identifier":"ENG-10","teamId":"TEAM","title":"Mapped issue"},"url":"https://linear.app/issue/ENG-10"});
    assert_eq!(
        request(&first, "linear", &issue, "mapped", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    let comment = |id: &str, text: &str| json!({"type":"Comment","action":"create","webhookTimestamp":now,"data":{"id":id,"issueId":"retired-issue","body":text},"url":"https://linear.app/issue/ENG-10#comment"});
    let partial = json!({"type":"Issue","action":"update","webhookTimestamp":now,"data":{"id":"retired-issue","title":"No scope information"},"url":"https://linear.app/issue/ENG-10"});
    assert_eq!(
        request(&first, "linear", &partial, "partial", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &first,
            "linear",
            &comment("partial-comment", "Still mapped"),
            "partial-comment",
            false,
            0,
            ""
        )
        .await
        .1["occurrences"],
        1
    );
    let published = messages(&store).len();
    let mut removed = issue.clone();
    removed["action"] = json!("update");
    removed["data"]["teamId"] = json!("UNMAPPED");
    store.lock().unwrap().fail_queries = 1;
    assert_eq!(
        request(&first, "linear", &removed, "lookup-fails", false, 0, "")
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    store.lock().unwrap().fail_record_prefix = Some("route:linear:issue:".into());
    assert_eq!(
        request(&first, "linear", &removed, "retirement-fails", false, 0, "")
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    // A later journal failure still leaves the retired route durable. The
    // failed delivery is retried instead of reporting an authoritative success.
    store.lock().unwrap().fail_record_prefix = Some("unmapped:linear:".into());
    assert_eq!(
        request(
            &first,
            "linear",
            &removed,
            "fingerprint-fails",
            false,
            0,
            ""
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let restarted = app(cfg, &keys);
    let response = request(
        &restarted,
        "linear",
        &comment("hidden", "Must not reach the old channel"),
        "hidden",
        false,
        0,
        "",
    )
    .await;
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.1["ignored"], true);
    assert_eq!(messages(&store).len(), published);
    assert_eq!(
        request(&restarted, "linear", &removed, "retired", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(messages(&store).len(), published);
    let mut returned = issue.clone();
    returned["action"] = json!("update");
    returned["data"]["title"] = json!("Returned to the mapped team");
    assert_eq!(
        request(&restarted, "linear", &returned, "returned", false, 0, "")
            .await
            .0,
        StatusCode::OK
    );
    // A completed unmapped update replay must not retire the restored route.
    assert_eq!(
        request(
            &restarted,
            "linear",
            &removed,
            "old-removal-replay",
            false,
            0,
            ""
        )
        .await
        .0,
        StatusCode::OK
    );
    let response = request(
        &restarted,
        "linear",
        &comment("visible", "Back in the mapped channel"),
        "visible",
        false,
        0,
        "",
    )
    .await;
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.1["occurrences"], 1);
    let visible = messages(&store)
        .into_iter()
        .find(|e| e.content.contains("Back in the mapped channel"))
        .unwrap();
    assert_eq!(tag(&visible, "h"), Some(CHANNEL));
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
