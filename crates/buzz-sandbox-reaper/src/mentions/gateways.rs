//! Read the owner's live delegated gateway keys (kind 30181) with the agent's
//! own relay identity. Every failure is an error, so callers fail closed: a
//! listener retries its session with backoff, and a launch is not started.

use super::{registry::Identity, Error, IO_TIMEOUT};
use buzz_sdk::gateway::Live;
use buzz_ws_client::{NostrWsConnection, RelayMessage};
use nostr::{Event, Timestamp};
use serde_json::{json, Value};

/// The owner's live gateways now, newest first, excluding the owner, this
/// agent, and any key whose profile claims agent identity.
///
/// The subscriptions are finite reads on a short-lived connection; they are
/// left to end with it.
pub(super) async fn read(
    connection: &mut NostrWsConnection,
    identity: &Identity,
) -> Result<Vec<Live>, Error> {
    tokio::time::timeout(IO_TIMEOUT, async {
        let now = Timestamp::now().as_secs();
        let heads = fetch(
            connection,
            "mention-gateways",
            json!({
                "kinds": [buzz_sdk::kind::KIND_AGENT_GATEWAY_DELEGATION],
                "authors": [identity.owner],
                "limit": buzz_sdk::gateway::LOOKUP_LIMIT,
            }),
            buzz_sdk::gateway::LOOKUP_LIMIT,
        )
        .await?;
        let candidates: Vec<String> = buzz_sdk::gateway::live_heads(identity.owner, &heads, now)
            .iter()
            .map(|live| live.gateway().to_hex())
            .collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let count = candidates.len();
        let profiles = fetch(
            connection,
            "mention-gateway-profiles",
            json!({"kinds": [0], "authors": candidates, "limit": count}),
            count,
        )
        .await?;
        Ok(buzz_sdk::gateway::resolve(
            identity.owner,
            &heads,
            &profiles,
            &[identity.owner, identity.keys.public_key()],
            now,
        ))
    })
    .await
    .map_err(|_| Error::Deadline)?
}

/// One finite REQ, read to EOSE. More than `limit` results is a relay fault.
async fn fetch(
    connection: &mut NostrWsConnection,
    id: &str,
    filter: Value,
    limit: usize,
) -> Result<Vec<Event>, Error> {
    let mut filter = filter;
    filter["consistency"] = json!("strong");
    connection
        .send_raw(&json!(["REQ", id, filter]))
        .await
        .map_err(|_| Error::Relay)?;
    let mut events = Vec::new();
    loop {
        match connection
            .next_event(IO_TIMEOUT)
            .await
            .map_err(|_| Error::Relay)?
        {
            RelayMessage::Event {
                subscription_id,
                event,
            } if subscription_id == id => {
                events.push(*event);
                if events.len() > limit {
                    return Err(Error::Relay);
                }
            }
            RelayMessage::Eose { subscription_id } if subscription_id == id => return Ok(events),
            RelayMessage::Closed {
                subscription_id, ..
            } if subscription_id == id => return Err(Error::Relay),
            RelayMessage::Auth { .. } => return Err(Error::Relay),
            _ => {}
        }
    }
}
