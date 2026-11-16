//! Agent-authenticated relay reads for one shared admission. No manager channel
//! membership or new relay read permission is required.

use crate::{mentions::Identity, mobile_launch::Registration};
use buzz_sdk::sharing::policy::{self, Task};
use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use k8s_openapi::api::core::v1::Secret;
use kube::Api;
use nostr::{Event, PublicKey, Timestamp};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub relay_url: String,
    pub trigger: Event,
}

/// Most events one admission read accepts. Above every policy read limit.
const MAX_QUERY_EVENTS: u64 = 64;

/// A failed verification and the stage that failed, for the manager's log.
#[derive(Debug)]
pub(crate) struct Failure {
    pub stage: &'static str,
    pub error: policy::Error,
}

fn at(stage: &'static str) -> impl FnOnce(policy::Error) -> Failure {
    move |error| Failure { stage, error }
}

pub(crate) async fn verify(
    client: kube::Client,
    namespace: &str,
    owner: PublicKey,
    agent: PublicKey,
    request: &Request,
    managed: &[policy::ManagedGateway],
) -> Result<Task, Failure> {
    let relay = crate::mobile_launch::relay_endpoint(&request.relay_url)
        .map_err(|_| policy::Error::Denied("invalid relay"))
        .map_err(at("relay_url"))?;
    let api: Api<Secret> = Api::namespaced(client, namespace);
    let secret = api
        .get_opt(&Registration::name(owner, agent, &relay))
        .await
        .map_err(|_| policy::Error::Unavailable)
        .and_then(|secret| {
            secret.ok_or(policy::Error::Denied(
                "start this agent once from desktop before sharing",
            ))
        })
        .map_err(at("agent_registration"))?;
    let identity = Identity::read(secret, namespace, owner, &relay)
        .ok()
        .filter(|identity| identity.keys.public_key() == agent)
        .ok_or(policy::Error::Unavailable)
        .map_err(at("agent_identity"))?;
    let relay_key = relay_key(&identity.relay).await.map_err(at("relay_info"))?;
    policy::authorize(
        |filter| query(&identity, filter),
        relay_key,
        owner,
        agent,
        &request.trigger,
        managed,
        Timestamp::now().as_secs(),
    )
    .await
    .map_err(at("policy"))
}

pub(crate) async fn relay_key(relay: &str) -> Result<PublicKey, policy::Error> {
    let mut url = url::Url::parse(relay).map_err(|_| policy::Error::Unavailable)?;
    let scheme = match url.scheme() {
        "wss" => "https",
        "ws" => "http",
        _ => return Err(policy::Error::Unavailable),
    };
    url.set_scheme(scheme)
        .map_err(|_| policy::Error::Unavailable)?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| policy::Error::Unavailable)?;
    let mut response = http
        .get(url)
        .header("accept", "application/nostr+json")
        .send()
        .await
        .map_err(|_| policy::Error::Unavailable)?
        .error_for_status()
        .map_err(|_| policy::Error::Unavailable)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| policy::Error::Unavailable)?
    {
        if bytes.len() + chunk.len() > 64 * 1024 {
            return Err(policy::Error::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    let info: Value = serde_json::from_slice(&bytes).map_err(|_| policy::Error::Unavailable)?;
    PublicKey::from_hex(info["self"].as_str().ok_or(policy::Error::Unavailable)?)
        .map_err(|_| policy::Error::Unavailable)
}

// Deliberately exclude Display/Debug error strings: relay frames and URLs can
// contain credentials or task text. Only fixed categories cross this boundary.
fn lookup_error(step: &'static str, error: &WsClientError, kinds: &Value) {
    let category = match error {
        WsClientError::Timeout | WsClientError::NoAuthChallenge => "timeout",
        WsClientError::ConnectionClosed => "connection_closed",
        WsClientError::AuthFailed(_) => "authentication_rejected",
        WsClientError::WebSocket(_) => "websocket",
        WsClientError::Json(_) | WsClientError::UnexpectedMessage(_) => "invalid_frame",
        _ => "client",
    };
    lookup_failed(step, category, kinds);
}

fn lookup_failed(step: &'static str, category: &'static str, kinds: &Value) {
    // The filter itself is not logged. Only numeric kind identifiers are safe.
    let kinds: Vec<u16> = kinds
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|kind| kind.as_u64().and_then(|kind| u16::try_from(kind).ok()))
        .take(64)
        .collect();
    tracing::warn!(
        lookup_step = step,
        lookup_error = category,
        ?kinds,
        "authorization lookup failed; task remains unauthorized and may be retried"
    );
}

async fn query(identity: &Identity, mut filter: Value) -> Result<Vec<Event>, ()> {
    // Admission must observe committed retirement before a daemon identity
    // Secret may be removed. Replica snapshots cannot supply that authority.
    filter["consistency"] = json!("strong");
    // More events than the filter's own limit is an ambiguous answer.
    let limit = filter["limit"]
        .as_u64()
        .filter(|limit| (1..=MAX_QUERY_EVENTS).contains(limit))
        .ok_or(())? as usize;
    let kinds = filter["kinds"].clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut connection = NostrWsConnection::connect_authenticated(
            &identity.relay,
            &identity.keys,
            Some(&identity.auth),
        )
        .await
        .map_err(|error| lookup_error("connect", &error, &kinds))?;
        connection
            .send_raw(&json!(["REQ", "shared-admission", filter]))
            .await
            .map_err(|error| lookup_error("send", &error, &kinds))?;
        let mut events = Vec::new();
        for _ in 0..limit + 8 {
            match connection
                .next_event(Duration::from_secs(10))
                .await
                .map_err(|error| lookup_error("receive", &error, &kinds))?
            {
                RelayMessage::Event {
                    subscription_id,
                    event,
                } if subscription_id == "shared-admission" => {
                    if events.len() >= limit {
                        lookup_failed("receive", "event_limit", &kinds);
                        return Err(());
                    }
                    events.push(*event);
                }
                RelayMessage::Eose { subscription_id } if subscription_id == "shared-admission" => {
                    return Ok(events)
                }
                RelayMessage::Closed { .. } => {
                    lookup_failed("receive", "subscription_closed", &kinds);
                    return Err(());
                }
                RelayMessage::Auth { .. } => {
                    lookup_failed("receive", "authentication_challenge", &kinds);
                    return Err(());
                }
                _ => {}
            }
        }
        lookup_failed("receive", "frame_limit", &kinds);
        Err(())
    })
    .await
    .map_err(|_| lookup_failed("query", "deadline", &kinds))?
}
