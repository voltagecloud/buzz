//! Owner-managed gateway delegations. Membership remains independently managed.

use buzz_sdk::gateway::{Delegation, MAX_DELEGATION_SECS};
use nostr::{Event, PublicKey, Timestamp};
use serde_json::json;

use crate::{client::BuzzClient, error::CliError, GatewayCmd};

/// Run a gateway command using the human owner's signing identity.
pub async fn dispatch(command: GatewayCmd, client: &BuzzClient) -> Result<(), CliError> {
    if client.auth_tag_owner_hex().is_some() {
        return Err(CliError::Auth(
            "gateway delegations require the owner's key".into(),
        ));
    }
    match command {
        GatewayCmd::List => {
            let (result, warning) = listed_gateways(client).await?;
            if let Some(warning) = warning {
                eprintln!("warning: {warning}");
            }
            println!("{result}");
            Ok(())
        }
        GatewayCmd::Grant { pubkey, expires } => {
            let response = grant(client, parse_key(&pubkey)?, duration(&expires)?).await?;
            println!("{response}");
            Ok(())
        }
        GatewayCmd::Revoke { pubkey } => {
            let gateway = parse_key(&pubkey)?;
            let created_at = next_timestamp(client, gateway, Timestamp::now().as_secs()).await?;
            let builder = Delegation::revoke(gateway, created_at)
                .map_err(|error| CliError::Usage(error.to_string()))?;
            println!("{}", publish(client, builder).await?);
            Ok(())
        }
    }
}

/// Publish an expiring grant for `gateway`, replacing its current head.
/// Returns the normalized write response. The caller must already have
/// refused NIP-OA agent credentials.
pub(crate) async fn grant(
    client: &BuzzClient,
    gateway: PublicKey,
    duration: u64,
) -> Result<String, CliError> {
    if gateway == client.keys().public_key() {
        return Err(CliError::Usage("the gateway must be a separate key".into()));
    }
    let now = Timestamp::now().as_secs();
    let created_at = next_timestamp(client, gateway, now).await?;
    let expires_at = now
        .checked_add(duration)
        .ok_or_else(|| CliError::Usage("gateway expiry is out of range".into()))?;
    let builder = Delegation::grant(gateway, expires_at, created_at)
        .map_err(|error| CliError::Usage(error.to_string()))?;
    publish(client, builder).await
}

const HEADROOM: usize = 4;
const LIMIT_WARNING: &str = concat!(
    "gateway authority checks only the newest 32 delegation heads, including disabled heads; ",
    "this list is near that limit. Renew any active gateway grant missing from the lookup window.",
);

async fn listed_gateways(client: &BuzzClient) -> Result<(String, Option<&'static str>), CliError> {
    let result = client
        .query(&json!({
            "consistency":"strong","kinds":[buzz_sdk::kind::KIND_AGENT_GATEWAY_DELEGATION],
            "authors":[client.keys().public_key()],"limit":buzz_sdk::gateway::LOOKUP_LIMIT,
        }))
        .await?;
    let values: serde_json::Value = serde_json::from_str(&result)
        .map_err(|_| CliError::Other("invalid gateway list response".into()))?;
    let heads = values
        .as_array()
        .ok_or_else(|| CliError::Other("invalid gateway list response".into()))?;
    let warning = (heads.len() >= buzz_sdk::gateway::LOOKUP_LIMIT.saturating_sub(HEADROOM))
        .then_some(LIMIT_WARNING);
    Ok((result, warning))
}

fn parse_key(value: &str) -> Result<PublicKey, CliError> {
    PublicKey::parse(value).map_err(|_| CliError::Usage("invalid gateway public key".into()))
}

fn duration(value: &str) -> Result<u64, CliError> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix('w') {
        (number, 7 * 86_400)
    } else if let Some(number) = value.strip_suffix('d') {
        (number, 86_400)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1)
    } else {
        return Err(CliError::Usage("use an expiry such as 24h or 90d".into()));
    };
    let seconds = number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier));
    match seconds {
        Some(seconds) if seconds > 0 && seconds <= MAX_DELEGATION_SECS => Ok(seconds),
        _ => Err(CliError::Usage(
            "gateway expiry must be positive and at most 90d".into(),
        )),
    }
}

/// A replacement must be newer than the head it replaces, including when
/// grant and revoke run during the same second. Refuse implausible future
/// heads rather than silently publishing a dominated revocation.
async fn next_timestamp(
    client: &BuzzClient,
    gateway: PublicKey,
    now: u64,
) -> Result<u64, CliError> {
    let result = client
        .query(&json!({
            "consistency": "strong",
                    "kinds": [buzz_sdk::kind::KIND_AGENT_GATEWAY_DELEGATION],
            "authors": [client.keys().public_key()],
            "#d": [gateway.to_hex()], "limit": 1,
        }))
        .await?;
    let heads: Vec<Event> = serde_json::from_str(&result)
        .map_err(|_| CliError::Other("invalid delegation head response".into()))?;
    if heads.len() > 1 {
        return Err(CliError::Other("too many delegation heads returned".into()));
    }
    let Some(head) = heads.first() else {
        return Ok(now);
    };
    let coordinate = head.tags.iter().find_map(|tag| match tag.as_slice() {
        [name, value, ..] if name == "d" => Some(value.as_str()),
        _ => None,
    });
    if head.pubkey != client.keys().public_key()
        || head.kind.as_u16() as u32 != buzz_sdk::kind::KIND_AGENT_GATEWAY_DELEGATION
        || coordinate != Some(gateway.to_hex().as_str())
        || head.verify().is_err()
    {
        return Err(CliError::Other(
            "relay returned an unrelated delegation head".into(),
        ));
    }
    let next = now.max(head.created_at.as_secs().saturating_add(1));
    if next > now.saturating_add(buzz_sdk::gateway::MAX_FUTURE_SKEW_SECS) {
        return Err(CliError::Conflict(
            "existing delegation is too far in the future; check the owner's clock".into(),
        ));
    }
    Ok(next)
}

async fn publish(client: &BuzzClient, builder: nostr::EventBuilder) -> Result<String, CliError> {
    let event = client.sign_event(builder)?;
    let response = client.submit_event(event).await?;
    super::parse_write_response(&response, "delegation changed concurrently; retry")
}

#[cfg(test)]
mod tests;
