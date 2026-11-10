//! Relay-backed write-ahead plans. Records are committed before readable messages.

use crate::{normalize::Occurrence, relay::Relay};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use nostr::{Event, EventBuilder, Kind, Tag, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MAX_PLAN: usize = 24 * 1024;
const REFRESH_AFTER: u64 = 14 * 60;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u8,
    payload_sha256: Option<String>,
    event: Event,
}

fn tag(name: &str, value: &str) -> Result<Tag> {
    Tag::parse([name, value]).context("invalid integration tag")
}
fn value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let p = tag.as_slice();
        (p.first().map(String::as_str) == Some(name))
            .then(|| p.get(1).map(String::as_str))
            .flatten()
    })
}

fn validate(relay: &Relay, channel: Uuid, plan: &Plan) -> Result<()> {
    if plan.version != 1
        || plan.event.verify().is_err()
        || plan.event.pubkey != relay.keys.public_key()
        || plan.event.kind.as_u16() != 9
        || value(&plan.event, "h") != Some(channel.to_string().as_str())
        || serde_json::to_vec(plan)?.len() > MAX_PLAN
    {
        bail!("invalid saved integration plan");
    }
    Ok(())
}

async fn read(relay: &Relay, channel: Uuid, coordinate: &str) -> Result<Option<Plan>> {
    let records=relay.query(json!({"kinds":[buzz_core::kind::KIND_INTEGRATION_RECORD],"authors":[relay.keys.public_key().to_hex()],"#d":[coordinate],"#h":[channel.to_string()],"limit":2})).await?;
    if records.len() > 1 {
        bail!("ambiguous integration record");
    }
    let Some(record) = records.first() else {
        return Ok(None);
    };
    if record.verify().is_err()
        || record.pubkey != relay.keys.public_key()
        || record.kind.as_u16() != 30623
        || value(record, "d") != Some(coordinate)
        || value(record, "h") != Some(channel.to_string().as_str())
        || record.content.len() > MAX_PLAN * 2
    {
        bail!("invalid integration record");
    }
    let bytes = STANDARD
        .decode(&record.content)
        .context("invalid plan encoding")?;
    if bytes.len() > MAX_PLAN {
        bail!("integration plan exceeded bound");
    }
    let plan: Plan = serde_json::from_slice(&bytes).context("invalid saved plan")?;
    validate(relay, channel, &plan)?;
    if value(record, "message") != Some(plan.event.id.to_hex().as_str()) {
        bail!("integration record points to a different event");
    }
    Ok(Some(plan))
}

async fn save(relay: &Relay, channel: Uuid, coordinate: &str, plan: &Plan) -> Result<()> {
    validate(relay, channel, plan)?;
    let body = STANDARD.encode(serde_json::to_vec(plan)?);
    let mut tags = vec![
        tag("d", coordinate)?,
        tag("h", &channel.to_string())?,
        tag("message", &plan.event.id.to_hex())?,
    ];
    tags.extend(
        plan.event
            .tags
            .iter()
            .filter(|tag| {
                tag.as_slice()
                    .first()
                    .is_some_and(|name| name.starts_with("ext-"))
            })
            .cloned(),
    );
    let event = EventBuilder::new(Kind::Custom(30623), body)
        .tags(tags)
        .sign_with_keys(&relay.keys)?;
    relay.submit(&event).await
}

async fn exists(relay: &Relay, channel: Uuid, event: &Event) -> Result<bool> {
    let events=relay.query(json!({"kinds":[9],"ids":[event.id.to_hex()],"authors":[relay.keys.public_key().to_hex()],"#h":[channel.to_string()],"limit":2})).await?;
    if events.len() > 1 {
        bail!("ambiguous exact event lookup");
    }
    match events.first() {
        None => Ok(false),
        Some(found)
            if found.id == event.id
                && found.verify().is_ok()
                && found.pubkey == event.pubkey
                && value(found, "h") == Some(channel.to_string().as_str()) =>
        {
            Ok(true)
        }
        Some(_) => bail!("invalid exact event lookup"),
    }
}

fn freshen(relay: &Relay, plan: &mut Plan) -> Result<()> {
    plan.event = EventBuilder::new(plan.event.kind, plan.event.content.clone())
        .tags(plan.event.tags.clone().to_vec())
        .custom_created_at(Timestamp::now())
        .sign_with_keys(&relay.keys)?;
    Ok(())
}

fn stale(plan: &Plan) -> bool {
    Timestamp::now()
        .as_secs()
        .abs_diff(plan.event.created_at.as_secs())
        > REFRESH_AFTER
}

/// Write one authenticated occurrence and its stable thread anchor. The global
/// HTTP pipeline serializes this across requests; deployment must use Recreate.
pub(crate) async fn publish(relay: &Relay, item: &Occurrence, body_hash: &str) -> Result<()> {
    let scope = hex::encode(Sha256::digest(format!(
        "{}:{}:{}",
        item.source, item.channel, item.thread
    )));
    let root_coord = format!("thread:{scope}");
    let mut root = match read(relay, item.channel, &root_coord).await? {
        Some(plan) => plan,
        None => Plan {
            version: 1,
            payload_sha256: None,
            event: EventBuilder::new(
                Kind::Custom(9),
                format!("{}: {}\n{}", item.source, item.reference, item.thread_url),
            )
            .tags([
                tag("h", &item.channel.to_string())?,
                tag("ext-thread", &item.thread)?,
                tag("ext-source", item.source)?,
            ])
            .sign_with_keys(&relay.keys)?,
        },
    };
    let root_exists = exists(relay, item.channel, &root.event).await?;
    if !root_exists {
        if stale(&root) {
            freshen(relay, &mut root)?;
        }
        save(relay, item.channel, &root_coord, &root).await?;
        if item.source == "linear" {
            save(
                relay,
                item.channel,
                &format!("route:linear:{}", item.thread),
                &root,
            )
            .await?;
        }
        relay.submit(&root.event).await?;
    }
    let delivery = format!("delivery:{scope}:{}", item.delivery);
    let fingerprint = format!("fingerprint:{scope}:{body_hash}");
    let a = read(relay, item.channel, &delivery).await?;
    let b = read(relay, item.channel, &fingerprint).await?;
    for plan in a.iter().chain(b.iter()) {
        if plan.payload_sha256.as_deref() != Some(body_hash) {
            bail!("delivery identity conflicts with saved payload");
        }
    }
    let mut plan = match (a, b) {
        (Some(a), Some(b)) if a.event.id != b.event.id => {
            if exists(relay, item.channel, &a.event).await? {
                a
            } else if exists(relay, item.channel, &b.event).await? {
                b
            } else if a.event.created_at >= b.event.created_at {
                a
            } else {
                b
            }
        }
        (Some(plan), _) | (_, Some(plan)) => plan,
        (None, None) => {
            let mut tags = vec![
                tag("h", &item.channel.to_string())?,
                Tag::parse(["e", &root.event.id.to_hex(), "", "root"])?,
                Tag::parse(["e", &root.event.id.to_hex(), "", "reply"])?,
            ];
            for (key, val) in &item.fields {
                tags.push(tag(&format!("ext-{key}"), val)?);
            }
            tags.push(tag("ext-key", &fingerprint)?);
            Plan {
                version: 1,
                payload_sha256: Some(body_hash.to_owned()),
                event: EventBuilder::new(Kind::Custom(9), &item.content)
                    .tags(tags)
                    .sign_with_keys(&relay.keys)?,
            }
        }
    };
    let accepted = exists(relay, item.channel, &plan.event).await?;
    if !accepted && stale(&plan) {
        // Only an authoritative successful empty lookup permits changing IDs.
        freshen(relay, &mut plan)?;
        let mut tags = plan.event.tags.clone().to_vec();
        tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("e"));
        tags.push(Tag::parse(["e", &root.event.id.to_hex(), "", "root"])?);
        tags.push(Tag::parse(["e", &root.event.id.to_hex(), "", "reply"])?);
        plan.event = EventBuilder::new(Kind::Custom(9), plan.event.content.clone())
            .tags(tags)
            .sign_with_keys(&relay.keys)?;
    }
    save(relay, item.channel, &delivery, &plan).await?;
    save(relay, item.channel, &fingerprint, &plan).await?;
    if !accepted {
        relay.submit(&plan.event).await?;
    }
    Ok(())
}

/// Resolve comments whose signed payload omits project/team information.
/// Explicit webhook mappings take precedence over this previously stored route.
pub(crate) async fn linear_channel(
    relay: &Relay,
    issue: &str,
    config: &crate::config::Config,
) -> Result<Option<Uuid>> {
    let coordinate = format!("route:linear:issue:{issue}");
    let records=relay.query(json!({"kinds":[30623],"authors":[relay.keys.public_key().to_hex()],"#d":[coordinate],"limit":2})).await?;
    if records.len() > 1 {
        bail!("ambiguous Linear issue route");
    }
    let Some(record) = records.first() else {
        return Ok(None);
    };
    if record.verify().is_err()
        || record.pubkey != relay.keys.public_key()
        || value(record, "d") != Some(coordinate.as_str())
    {
        bail!("invalid Linear issue route");
    }
    let channel = value(record, "h")
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| anyhow::anyhow!("invalid issue route channel"))?;
    let allowed = config
        .linear_projects
        .values()
        .chain(config.linear_teams.values())
        .chain(config.linear_webhooks.values())
        .any(|known| *known == channel);
    if !allowed {
        bail!("Linear issue route is outside configured channels");
    }
    Ok(Some(channel))
}
