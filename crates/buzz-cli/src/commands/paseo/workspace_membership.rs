//! Idempotent membership preparation. Complete all adds before publishing a
//! workspace; propagate partial failures so rerunning create can finish them.
use crate::{
    client::{extract_d_tag, BuzzClient},
    error::CliError,
};
use nostr::{Event, PublicKey, Timestamp};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

const READ_BOUND: u32 = 2_000;

async fn channel_ids(client: &BuzzClient, key: PublicKey) -> Result<HashMap<Uuid, bool>, CliError> {
    let events = client
        .query_all_bounded(
            json!({"consistency":"strong","kinds":[39002],"#p":[key]}),
            READ_BOUND,
        )
        .await?;
    Ok(events
        .iter()
        .filter(|e| {
            e["kind"] == 39002
                && e["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|t| t[0] == "p" && t[1] == key.to_hex()))
        })
        .filter_map(|e| {
            let channel = Uuid::parse_str(&extract_d_tag(e)).ok()?;
            let entries: Vec<_> = e["tags"]
                .as_array()?
                .iter()
                .filter(|tag| tag[0] == "p" && tag[1] == key.to_hex())
                .collect();
            let posting = entries.len() == 1
                && entries[0][3]
                    .as_str()
                    .and_then(|role| role.parse::<buzz_core::channel::MemberRole>().ok())
                    .is_some_and(|role| role.can_post());
            Some((channel, posting))
        })
        .collect())
}

async fn posting_member(
    client: &BuzzClient,
    channel: Uuid,
    key: PublicKey,
) -> Result<bool, CliError> {
    let info: serde_json::Value = serde_json::from_str(&client.get_public("/").await?)
        .map_err(|e| CliError::Other(format!("invalid relay info: {e}")))?;
    let relay = info["self"]
        .as_str()
        .and_then(|s| PublicKey::parse(s).ok())
        .ok_or_else(|| CliError::Other("relay info missing valid self key".into()))?;
    let raw = client.query(&json!({"consistency":"strong", "kinds":[39002], "authors":[relay], "#d":[channel.to_string()], "limit":1})).await?;
    let events: Vec<Event> = serde_json::from_str(&raw)
        .map_err(|e| CliError::Other(format!("invalid current roster: {e}")))?;
    if events.len() > 1 {
        return Err(CliError::Other("multiple current rosters returned".into()));
    }
    let Some(event) = events.first() else {
        return Ok(false);
    };
    if event.pubkey != relay
        || event.kind.as_u16() != 39002
        || event.tags.identifier() != Some(channel.to_string().as_str())
        || event.verify().is_err()
    {
        return Err(CliError::Other("untrusted current roster".into()));
    }
    let key = key.to_hex();
    let entries: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| {
            let parts = tag.as_slice();
            parts.first().is_some_and(|p| p == "p") && parts.get(1) == Some(&key)
        })
        .collect();
    Ok(entries.len() == 1
        && entries[0]
            .as_slice()
            .get(3)
            .and_then(|role| role.parse::<buzz_core::channel::MemberRole>().ok())
            .is_some_and(|role| role.can_post()))
}

pub(super) async fn ensure(
    client: &BuzzClient,
    channel: Uuid,
    explicit_agents: &[PublicKey],
) -> Result<Vec<String>, CliError> {
    let owner = client.keys().public_key();
    if !posting_member(client, channel, owner).await? {
        return Err(CliError::Usage(
            "the owner must be a posting member of the workspace channel".into(),
        ));
    }
    let gateways = buzz_sdk::gateway::lookup(
        |mut filter| async move {
            filter["consistency"] = json!("strong");
            let raw = client.query(&filter).await?;
            serde_json::from_str::<Vec<Event>>(&raw)
                .map_err(|e| CliError::Other(format!("invalid gateway response: {e}")))
        },
        owner,
        &[owner],
        Timestamp::now().as_secs(),
    )
    .await?;
    for gateway in gateways {
        if !posting_member(client, channel, gateway.gateway()).await? {
            super::channels::add_member(client,channel,&gateway.gateway().to_hex()).await.map_err(|e|
                CliError::Other(format!("gateway membership incomplete: {e}; rerun this command to finish; existing memberships are preserved")))?;
            if !posting_member(client, channel, gateway.gateway()).await? {
                return Err(CliError::Conflict(
                    "gateway membership was not confirmed; rerun to finish".into(),
                ));
            }
        }
    }
    let listings = client
        .query_all_bounded(
            json!({"consistency":"strong","kinds":[30177],"authors":[owner]}),
            READ_BOUND,
        )
        .await?;
    let mut agents: HashSet<PublicKey> = explicit_agents.iter().copied().collect();
    for value in listings {
        let event: Event = serde_json::from_value(value)
            .map_err(|e| CliError::Other(format!("invalid agent listing: {e}")))?;
        if event.pubkey == owner && event.kind.as_u16() == 30177 && event.verify().is_ok() {
            if let Some(key) = event
                .tags
                .identifier()
                .and_then(|s| PublicKey::from_hex(s).ok())
            {
                agents.insert(key);
            }
        }
    }
    let mut agents: Vec<_> = agents.into_iter().collect();
    agents.sort_by_key(PublicKey::to_hex);
    let mut warnings = Vec::new();
    for agent in agents {
        let channels = channel_ids(client, agent).await?;
        if channels.get(&channel) != Some(&true) {
            warnings.push(format!(
                "agent {} is not a posting member of channel {channel}; add it before starting a session",
                agent.to_hex()
            ));
        }
        if channels.len() > 64 {
            warnings.push(format!("agent {} belongs to {} channels; the mention launcher watches at most 64 and may miss this workspace",agent.to_hex(),channels.len()));
        }
    }
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    Ok(warnings)
}
