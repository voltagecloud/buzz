//! Agent-authenticated relay reads for one shared admission. No manager channel
//! membership or new relay read permission is required.

use crate::{mentions::Identity, mobile_launch::Registration};
use buzz_sdk::sharing::policy::{self, Task};
use buzz_ws_client::{NostrWsConnection, RelayMessage};
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

pub(crate) async fn verify(
    client: kube::Client,
    namespace: &str,
    owner: PublicKey,
    agent: PublicKey,
    request: &Request,
) -> Result<Task, policy::Error> {
    let relay = crate::mobile_launch::relay_endpoint(&request.relay_url)
        .map_err(|_| policy::Error::Denied("invalid relay"))?;
    let api: Api<Secret> = Api::namespaced(client, namespace);
    let secret = api
        .get_opt(&Registration::name(owner, agent, &relay))
        .await
        .map_err(|_| policy::Error::Unavailable)?
        .ok_or(policy::Error::Denied(
            "start this agent once from desktop before sharing",
        ))?;
    let identity =
        Identity::read(secret, namespace, owner, &relay).map_err(|_| policy::Error::Unavailable)?;
    if identity.keys.public_key() != agent {
        return Err(policy::Error::Unavailable);
    }
    let relay_key = relay_key(&identity.relay).await?;
    policy::authorize(
        |filter| query(&identity, filter),
        relay_key,
        owner,
        agent,
        &request.trigger,
        Timestamp::now().as_secs(),
    )
    .await
}

async fn relay_key(relay: &str) -> Result<PublicKey, policy::Error> {
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

async fn query(identity: &Identity, filter: Value) -> Result<Vec<Event>, ()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut connection = NostrWsConnection::connect_authenticated(
            &identity.relay,
            &identity.keys,
            Some(&identity.auth),
        )
        .await
        .map_err(|_| ())?;
        connection
            .send_raw(&json!(["REQ", "shared-admission", filter]))
            .await
            .map_err(|_| ())?;
        let mut events = Vec::new();
        for _ in 0..8 {
            match connection
                .next_event(Duration::from_secs(10))
                .await
                .map_err(|_| ())?
            {
                RelayMessage::Event {
                    subscription_id,
                    event,
                } if subscription_id == "shared-admission" => {
                    if !events.is_empty() {
                        return Err(());
                    }
                    events.push(*event);
                }
                RelayMessage::Eose { subscription_id } if subscription_id == "shared-admission" => {
                    return Ok(events)
                }
                RelayMessage::Closed { .. } | RelayMessage::Auth { .. } => return Err(()),
                _ => {}
            }
        }
        Err(())
    })
    .await
    .map_err(|_| ())?
}
