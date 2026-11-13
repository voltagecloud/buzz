//! Interim channel sharing for the daemon's gateway, until the relay lets a
//! live gateway inherit its owner's channel access.
//!
//! The gateway joins every stream and forum channel the owner is in, open or
//! private, except excluded ones. Direct messages and system channels are
//! never shared: gateway delegation excludes owner DMs, and adding a third key
//! to a DM changes the other participant's conversation.

use std::collections::HashSet;

use buzz_core::kind::KIND_NIP43_MEMBERSHIP_LIST;
use nostr::PublicKey;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::client::{extract_d_tag, BuzzClient};
use crate::error::CliError;

/// Membership and metadata reads fail visibly above this many events rather
/// than treating a truncated listing as complete.
const READ_BOUND: u32 = 2_000;

/// Channel types the gateway may join. Unknown types fail closed.
const SHARED_TYPES: [&str; 2] = ["stream", "forum"];

/// One channel the owner belongs to, from its relay-signed kind:39000.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Channel {
    pub id: Uuid,
    pub name: String,
    /// The first `t` tag: `stream`, `forum`, `dm`, `system`, ...
    pub channel_type: String,
    pub private: bool,
    pub archived: bool,
}

impl Channel {
    pub(super) fn parse(event: &Value) -> Option<Self> {
        let tags = event.get("tags")?.as_array()?;
        let values = |tag: &Value| -> Vec<String> {
            tag.as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let tags: Vec<Vec<String>> = tags.iter().map(values).collect();
        let value = |name: &str| {
            tags.iter()
                .find(|tag| tag.first().map(String::as_str) == Some(name))
                .and_then(|tag| tag.get(1).cloned())
        };
        Some(Self {
            id: Uuid::parse_str(&extract_d_tag(event)).ok()?,
            name: value("name").unwrap_or_default(),
            channel_type: value("t")?,
            private: tags.iter().any(|tag| tag.len() == 1 && tag[0] == "private"),
            archived: value("archived").as_deref() == Some("true"),
        })
    }

    fn summary(&self) -> Value {
        json!({"channel_id": self.id, "name": self.name, "private": self.private})
    }
}

/// What sharing will change, decided without any I/O.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Plan {
    pub add: Vec<Channel>,
    pub present: Vec<Channel>,
    pub excluded: Vec<Channel>,
    /// Excluded channels the gateway already belongs to. Removing another
    /// member can need channel administration, so sharing only reports them.
    pub excluded_present: Vec<Channel>,
    /// Channels never shared: direct messages, system or unknown types, and
    /// archived channels.
    pub skipped: usize,
}

/// Decide channel changes. Every `exclude` entry must match a channel by
/// UUID or exact name, so a typo cannot silently share a channel, except an
/// entry naming `pending`: a channel about to be created. That entry still
/// excludes the channel whenever it exists.
pub(super) fn plan(
    channels: Vec<Channel>,
    gateway_in: &HashSet<Uuid>,
    exclude: &[String],
    pending: Option<&str>,
) -> Result<Plan, CliError> {
    let matches = |channel: &Channel, entry: &str| {
        Uuid::parse_str(entry).is_ok_and(|id| id == channel.id)
            || buzz_core::channel::canonical_channel_name(entry) == channel.name
    };
    let names_pending = |entry: &str| {
        pending.is_some_and(|name| buzz_core::channel::canonical_channel_name(entry) == name)
    };
    if let Some(unknown) = exclude.iter().find(|entry| {
        !names_pending(entry) && !channels.iter().any(|channel| matches(channel, entry))
    }) {
        return Err(CliError::Usage(format!(
            "--exclude {unknown:?} matches no channel you are in"
        )));
    }
    let mut plan = Plan::default();
    for channel in channels {
        if !SHARED_TYPES.contains(&channel.channel_type.as_str()) || channel.archived {
            plan.skipped += 1;
        } else if exclude.iter().any(|entry| matches(&channel, entry)) {
            if gateway_in.contains(&channel.id) {
                plan.excluded_present.push(channel.clone());
            }
            plan.excluded.push(channel);
        } else if gateway_in.contains(&channel.id) {
            plan.present.push(channel);
        } else {
            plan.add.push(channel);
        }
    }
    for list in [
        &mut plan.add,
        &mut plan.present,
        &mut plan.excluded,
        &mut plan.excluded_present,
    ] {
        list.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    }
    Ok(plan)
}

/// Share the owner's channels with `gateway` and return the report. A
/// gateway the community has not admitted yet gets no channels; the report
/// names the operator command instead.
pub(super) async fn share(
    client: &BuzzClient,
    gateway: PublicKey,
    exclude: &[String],
    pending: Option<&str>,
) -> Result<Value, CliError> {
    let gateway_hex = gateway.to_hex();
    if !admitted(client, &gateway_hex).await? {
        return Ok(json!({
            "admitted": false,
            "admin_command": format!("buzz-admin add-member --pubkey {gateway_hex} --role member"),
        }));
    }
    let owner_ids = channel_ids(client, &client.keys().public_key().to_hex()).await?;
    let gateway_in: HashSet<Uuid> = channel_ids(client, &gateway_hex)
        .await?
        .into_iter()
        .collect();
    let channels = if owner_ids.is_empty() {
        Vec::new()
    } else {
        let ids: Vec<String> = owner_ids.iter().map(Uuid::to_string).collect();
        client
            .query_all_bounded(json!({"kinds": [39000], "#d": ids}), READ_BOUND)
            .await?
            .iter()
            .filter_map(Channel::parse)
            .filter(|channel| owner_ids.contains(&channel.id))
            .collect()
    };
    let plan = plan(channels, &gateway_in, exclude, pending)?;
    let (mut added, mut failed) = (Vec::new(), Vec::new());
    for channel in &plan.add {
        match add_member(client, channel.id, &gateway_hex).await {
            Ok(()) => added.push(channel.summary()),
            Err(error) => failed.push(json!({
                "channel_id": channel.id, "name": channel.name, "error": error.to_string(),
            })),
        }
    }
    let list =
        |channels: &[Channel]| -> Vec<Value> { channels.iter().map(Channel::summary).collect() };
    Ok(json!({
        "admitted": true,
        "added": added,
        "already_present": list(&plan.present),
        "excluded": list(&plan.excluded),
        // Still members: remove them with `buzz channels remove-member`.
        "excluded_but_present": list(&plan.excluded_present),
        "skipped": plan.skipped,
        "failed": failed,
    }))
}

/// Whether the relay's NIP-43 member list names `gateway`. A relay that
/// publishes no list is treated as admitting it and refuses writes itself.
async fn admitted(client: &BuzzClient, gateway: &str) -> Result<bool, CliError> {
    let response = client
        .query(&json!({"kinds": [KIND_NIP43_MEMBERSHIP_LIST], "limit": 1}))
        .await?;
    let lists: Vec<Value> = serde_json::from_str(&response)
        .map_err(|_| CliError::Other("invalid community member list response".into()))?;
    let Some(list) = lists.first() else {
        return Ok(true);
    };
    Ok(list["tags"].as_array().is_some_and(|tags| {
        tags.iter().any(|tag| {
            tag.get(0).and_then(Value::as_str) == Some("member")
                && tag.get(1).and_then(Value::as_str) == Some(gateway)
        })
    }))
}

/// Channels whose current kind:39002 roster lists `pubkey`.
async fn channel_ids(client: &BuzzClient, pubkey: &str) -> Result<Vec<Uuid>, CliError> {
    Ok(client
        .query_all_bounded(json!({"kinds": [39002], "#p": [pubkey]}), READ_BOUND)
        .await?
        .iter()
        .filter_map(|event| Uuid::parse_str(&extract_d_tag(event)).ok())
        .collect())
}

async fn add_member(client: &BuzzClient, channel: Uuid, gateway: &str) -> Result<(), CliError> {
    let builder = buzz_sdk::build_add_member(channel, gateway, Some(buzz_sdk::MemberRole::Member))
        .map_err(|error| CliError::Other(format!("build_add_member failed: {error}")))?;
    let response = client.submit_event(client.sign_event(builder)?).await?;
    super::super::parse_write_response(&response, "membership changed concurrently; rerun")?;
    Ok(())
}

#[cfg(test)]
#[path = "channels_tests.rs"]
mod tests;
