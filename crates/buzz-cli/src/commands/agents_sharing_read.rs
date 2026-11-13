//! Sharing reads bind diagnostics to the signed agent profile. This read-only
//! resolution does not grant invocation, ownership, or lifecycle authority.
use super::*;
use nostr::Event;
use serde_json::Value;

async fn query(client: &BuzzClient, mut filter: Value) -> Result<Vec<Event>, CliError> {
    filter["consistency"] = json!("strong");
    let raw = client.query(&filter).await?;
    serde_json::from_str(&raw)
        .map_err(|_| CliError::Other("invalid sharing lookup response".into()))
}

pub(super) async fn read(
    client: &BuzzClient,
    agent: &str,
    channel: uuid::Uuid,
    requested: Option<&str>,
) -> Result<Value, CliError> {
    validate_hex64(agent)?;
    let agent = PublicKey::from_hex(agent).map_err(|e| CliError::Usage(e.to_string()))?;
    let requested = requested
        .map(|key| {
            validate_hex64(key)?;
            PublicKey::from_hex(key).map_err(|e| CliError::Usage(e.to_string()))
        })
        .transpose()?;
    let profiles = query(client, json!({"kinds":[0],"authors":[agent],"limit":1})).await?;
    let now = nostr::Timestamp::now().as_secs();
    let profile = profiles
        .first()
        .filter(|profile| profiles.len() == 1 && profile.pubkey == agent)
        .ok_or_else(|| {
            CliError::Other("cannot resolve sharing owner: agent profile unavailable".into())
        })?;
    let resolved = buzz_sdk::sharing::profile_owner(profile, now).map_err(|_| {
        CliError::Auth("cannot resolve sharing owner: agent owner attestation is invalid".into())
    })?;
    let (query_owner, resolution) = match requested {
        None => (resolved, "agent_profile"),
        Some(key) if key == resolved => (resolved, "explicit_owner"),
        Some(key) => {
            let gateways = buzz_sdk::gateway::lookup(
                |filter| query(client, filter),
                resolved,
                &[resolved],
                now,
            )
            .await?;
            if gateways.iter().any(|gateway| gateway.gateway() == key) {
                (resolved, "delegated_gateway")
            } else {
                (key, "explicit_owner_mismatch")
            }
        }
    };
    let grants = query(
        client,
        json!({"kinds":[buzz_core::kind::KIND_AGENT_CHANNEL_GRANT],
        "authors":[query_owner],"#h":[channel],
        "#d":[buzz_sdk::sharing::Grant::coordinate(agent, channel)],"limit":1}),
    )
    .await?;
    Ok(json!({
        "agent":agent, "channel":channel,
        "signing_key":client.keys().public_key(), "requested_owner":requested,
        "resolved_owner":resolved, "query_owner":query_owner,
        "owner_resolution":resolution, "grants":grants,
        "warning":(query_owner != resolved).then_some(
            "query key differs from the verified agent owner; empty results do not establish an absent owner grant")
    }))
}
