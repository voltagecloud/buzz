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
    fail_event_lookup: Option<String>,
    lose_root_response: bool,
    submissions: Vec<Event>,
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
    let filters: Value = serde_json::from_slice(&body).unwrap();
    let filter = &filters[0];
    assert_eq!(filter["consistency"], "strong");
    let mut store = store.lock().unwrap();
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
        if event.kind.as_u16() == 30623
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
    let event: Event = serde_json::from_slice(&body).unwrap();
    event.verify().unwrap();
    let mut store = store.lock().unwrap();
    store.submissions.push(event.clone());
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
    if let Some(d) = tag(&event, "d") {
        store.heads.insert(d.to_owned(), event.id.to_hex());
    }
    store.events.insert(event.id.to_hex(), event.clone());
    if event.kind.as_u16() == 9 && tag(&event, "e").is_none() && store.lose_root_response {
        store.lose_root_response = false;
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    (StatusCode::OK, Json(json!(["OK", event.id, true, ""])))
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
    let issue = json!({"type":"Issue","action":"create","webhookTimestamp":now,"webhookId":"HOOK","url":"https://linear.app/issue/ENG-7","actor":{"name":"Ada"},"data":{"id":"issue-7","identifier":"ENG-7","teamId":"TEAM","title":"Fix it","description":"Details"}});
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
    let comment = json!({"type":"Comment","action":"create","webhookTimestamp":now,"webhookId":"HOOK","url":"https://linear.app/issue/ENG-7#comment-1","actor":{"name":"Ada"},"data":{"id":"comment-1","issueId":"issue-7","body":"An update"}});
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
    server.abort();
}

#[test]
fn endpoints_and_provider_mappings_fail_closed() {
    let mut cfg = config("https://buzz.voltage.cloud");
    cfg.validate().unwrap();
    cfg.relay_url = "http://public.example".into();
    assert!(cfg.validate().is_err());
    cfg.relay_url = "https://buzz.voltage.cloud".into();
    cfg.slack_channels
        .insert("T123/GPRIVATE".into(), CHANNEL.parse().unwrap());
    assert!(cfg.validate().is_err());
}
