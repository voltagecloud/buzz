//! Bound the wake subscription to the addressed agent's current channels.
//! On Buzz, an unscoped live subscription routes global events only.

use super::{registry::Identity, Error, IO_TIMEOUT};
use buzz_ws_client::{NostrWsConnection, RelayMessage};
use serde_json::json;
use std::collections::BTreeSet;
use uuid::Uuid;

const MAX_CHANNELS: usize = 64;

pub(super) async fn read(
    connection: &mut NostrWsConnection,
    identity: &Identity,
) -> Result<Vec<Uuid>, Error> {
    tokio::time::timeout(IO_TIMEOUT, async {
        // Kind 39002 is relay-authored channel membership metadata. Its
        // coordinate is d, not h. No manager membership is needed to read it.
        connection
            .send_raw(&json!(["REQ","mention-channels",{
                "kinds":[39002],"#p":[identity.keys.public_key()],"limit":MAX_CHANNELS+1
            }]))
            .await
            .map_err(|_| Error::Relay)?;
        let mut channels = BTreeSet::new();
        for _ in 0..MAX_CHANNELS + 2 {
            match connection
                .next_event(IO_TIMEOUT)
                .await
                .map_err(|_| Error::Relay)?
            {
                RelayMessage::Event {
                    subscription_id,
                    event,
                } if subscription_id == "mention-channels" => {
                    if event.kind.as_u16() != 39002
                        || event.verify().is_err()
                        || !event.tags.iter().any(|tag| {
                            let t = tag.as_slice();
                            t.first().map(String::as_str) == Some("p")
                                && t.get(1) == Some(&identity.keys.public_key().to_hex())
                        })
                    {
                        return Err(Error::Relay);
                    }
                    let d = buzz_sdk::sharing::task::single_tag(&event, "d", 2)
                        .map_err(|_| Error::Relay)?
                        .ok_or(Error::Relay)?;
                    let channel = Uuid::parse_str(&d[1]).map_err(|_| Error::Relay)?;
                    if channel.to_string() != d[1] {
                        return Err(Error::Relay);
                    }
                    channels.insert(channel);
                    if channels.len() > MAX_CHANNELS {
                        return Err(Error::Capacity);
                    }
                }
                RelayMessage::Eose { subscription_id } if subscription_id == "mention-channels" => {
                    connection
                        .send_raw(&json!(["CLOSE", "mention-channels"]))
                        .await
                        .map_err(|_| Error::Relay)?;
                    return Ok(channels.into_iter().collect());
                }
                RelayMessage::Closed { .. } | RelayMessage::Auth { .. } => {
                    return Err(Error::Relay)
                }
                _ => {}
            }
        }
        Err(Error::Capacity)
    })
    .await
    .map_err(|_| Error::Deadline)?
}
