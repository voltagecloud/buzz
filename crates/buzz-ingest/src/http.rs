//! Signature verification, bounded admission, and webhook acknowledgements.

use crate::{
    config::{Config, Secrets},
    normalize, plan,
    relay::Relay,
};
use axum::{
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};

const MAX_BODY: usize = 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(4);
const RATE: usize = 20;
const AGE: u64 = 300;

type Error = (StatusCode, &'static str);
type Result<T> = std::result::Result<T, Error>;

struct Bucket {
    start: Instant,
    count: usize,
}
struct App {
    config: Config,
    secrets: Secrets,
    relay: Relay,
    pipeline: Semaphore,
    readiness: Semaphore,
    rates: Mutex<[Bucket; 3]>,
}

pub(crate) fn router(config: Config, secrets: Secrets, relay: Relay) -> Router {
    let now = Instant::now();
    let app = Arc::new(App {
        config,
        secrets,
        relay,
        pipeline: Semaphore::new(1),
        readiness: Semaphore::new(1),
        rates: Mutex::new(std::array::from_fn(|_| Bucket {
            start: now,
            count: 0,
        })),
    });
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ready", get(ready))
        .route("/ingest/{provider}", post(deliver))
        .with_state(app)
}

async fn ready(State(app): State<Arc<App>>) -> StatusCode {
    let Ok(_permit) = app.readiness.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    if app.relay.ready().await.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or((StatusCode::UNAUTHORIZED, "missing signature metadata"))
}

fn signature(secret: &str, signed: &[u8], raw: &str) -> Result<()> {
    let supplied = hex::decode(raw).map_err(|_| (StatusCode::UNAUTHORIZED, "invalid signature"))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "signature verifier unavailable",
        )
    })?;
    mac.update(signed);
    mac.verify_slice(&supplied)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid signature"))
}

fn delivery_id(raw: &str) -> Result<&str> {
    if raw.is_empty()
        || raw.len() > 128
        || !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err((StatusCode::BAD_REQUEST, "invalid delivery id"));
    }
    Ok(raw)
}

async fn deliver(
    State(app): State<Arc<App>>,
    Path(provider): Path<String>,
    request: Request,
) -> Response {
    let source = match provider.as_str() {
        "github" => 0,
        "linear" => 1,
        "slack" => 2,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let secret = match source {
        0 => app.secrets.github.as_deref(),
        1 => app.secrets.linear.as_deref(),
        _ => app.secrets.slack.as_deref(),
    };
    let Some(secret) = secret else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(_permit) = app.pipeline.try_acquire() else {
        return (StatusCode::TOO_MANY_REQUESTS, "ingestion busy; retry").into_response();
    };
    {
        let mut rates = app.rates.lock().await;
        let bucket = &mut rates[source];
        if bucket.start.elapsed() >= Duration::from_secs(1) {
            bucket.start = Instant::now();
            bucket.count = 0;
        }
        if bucket.count >= RATE {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "provider rate exceeded; retry",
            )
                .into_response();
        }
        bucket.count += 1;
    }
    let result = tokio::time::timeout(DEADLINE, process(&app, source, secret, request)).await;
    match result {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(error)) => error.into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "ingestion deadline reached; retry",
        )
            .into_response(),
    }
}

async fn process(app: &App, source: usize, secret: &str, request: Request) -> Result<Value> {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| (StatusCode::PAYLOAD_TOO_LARGE, "body exceeds 1 MiB"))?;
    let now = nostr::Timestamp::now().as_secs();
    let mut slack_delivery = None;
    match source {
        0 => signature(
            secret,
            &body,
            header(&parts.headers, "x-hub-signature-256")?
                .strip_prefix("sha256=")
                .ok_or((StatusCode::UNAUTHORIZED, "invalid signature format"))?,
        )?,
        1 => signature(secret, &body, header(&parts.headers, "linear-signature")?)?,
        _ => {
            let raw_time = header(&parts.headers, "x-slack-request-timestamp")?;
            let time = raw_time
                .parse::<u64>()
                .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid timestamp"))?;
            if now.abs_diff(time) > AGE {
                return Err((StatusCode::UNAUTHORIZED, "stale webhook"));
            }
            let mut signed = format!("v0:{raw_time}:").into_bytes();
            signed.extend_from_slice(&body);
            signature(
                secret,
                &signed,
                header(&parts.headers, "x-slack-signature")?
                    .strip_prefix("v0=")
                    .ok_or((StatusCode::UNAUTHORIZED, "invalid signature format"))?,
            )?;
        }
    }
    let payload: Value =
        serde_json::from_slice(&body).map_err(|_| (StatusCode::BAD_REQUEST, "invalid JSON"))?;
    if source == 1 {
        let time = payload
            .get("webhookTimestamp")
            .and_then(Value::as_u64)
            .ok_or((StatusCode::UNAUTHORIZED, "missing signed timestamp"))?
            / 1000;
        if now.abs_diff(time) > AGE {
            return Err((StatusCode::UNAUTHORIZED, "stale webhook"));
        }
    }
    if source == 2 {
        if payload.get("type").and_then(Value::as_str) == Some("url_verification") {
            let challenge = payload
                .get("challenge")
                .and_then(Value::as_str)
                .filter(|v| v.len() <= 256)
                .ok_or((StatusCode::BAD_REQUEST, "invalid challenge"))?;
            return Ok(json!({"challenge":challenge}));
        }
        slack_delivery = payload.get("event_id").and_then(Value::as_str);
    }
    let delivery = delivery_id(match source {
        0 => header(&parts.headers, "x-github-delivery")?,
        1 => header(&parts.headers, "linear-delivery")?,
        _ => slack_delivery.ok_or((StatusCode::BAD_REQUEST, "missing Slack event_id"))?,
    })?;
    let items = match source {
        0 => normalize::github(
            &app.config,
            &payload,
            delivery,
            header(&parts.headers, "x-github-event")?,
        ),
        1 => {
            let explicit = payload
                .pointer("/data/teamId")
                .and_then(Value::as_str)
                .is_some_and(|id| app.config.linear_teams.contains_key(id))
                || payload
                    .pointer("/data/projectId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| app.config.linear_projects.contains_key(id));
            let route = if !explicit {
                if let Some(issue) = normalize::linear_issue(&payload) {
                    plan::linear_channel(&app.relay, issue, &app.config)
                        .await
                        .map_err(|_| {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                "issue route lookup failed; retry",
                            )
                        })?
                } else {
                    None
                }
            } else {
                None
            };
            normalize::linear(&app.config, &payload, delivery, route)
        }
        _ => normalize::slack(&app.config, &payload, delivery),
    }
    .map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "unsupported or malformed mapped provider payload",
        )
    })?;
    // Linear can refresh the signed transport timestamp on redelivery. It
    // must not create a second occurrence of the same authenticated payload.
    let mut identity_payload = payload.clone();
    if source == 1 {
        if let Some(object) = identity_payload.as_object_mut() {
            object.remove("webhookTimestamp");
        }
    }
    let identity = serde_json::to_vec(&identity_payload)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid payload identity"))?;
    let body_hash = hex::encode(Sha256::digest(identity));
    for item in &items {
        plan::publish(&app.relay, item, &body_hash)
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "relay write incomplete; retry delivery",
                )
            })?;
    }
    Ok(json!({"accepted":true,"occurrences":items.len(),"ignored":items.is_empty()}))
}
