//! Relay-backed write-ahead plans. Records are committed before readable messages.

use crate::{config::Config, normalize::Occurrence, relay::Relay};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use buzz_core::{integration, kind::KIND_INTEGRATION_RECORD};
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
    let records=relay.query(json!({"kinds":[KIND_INTEGRATION_RECORD],"authors":[relay.keys.public_key().to_hex()],"#d":[coordinate],"#h":[channel.to_string()],"limit":2})).await?;
    if records.len() > 1 {
        bail!("ambiguous integration record");
    }
    let Some(record) = records.first() else {
        return Ok(None);
    };
    decode(relay, channel, coordinate, record).map(Some)
}

fn decode(relay: &Relay, channel: Uuid, coordinate: &str, record: &Event) -> Result<Plan> {
    if record.verify().is_err()
        || record.pubkey != relay.keys.public_key()
        || record.kind.as_u16() != KIND_INTEGRATION_RECORD as u16
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
    Ok(plan)
}

/// The current record at `coordinate` in any channel, verified as this
/// publisher's own. Read errors propagate.
async fn head(relay: &Relay, coordinate: &str) -> Result<Option<Event>> {
    let records=relay.query(json!({"kinds":[KIND_INTEGRATION_RECORD],"authors":[relay.keys.public_key().to_hex()],"#d":[coordinate],"limit":2})).await?;
    if records.len() > 1 {
        bail!("ambiguous integration record");
    }
    let Some(record) = records.into_iter().next() else {
        return Ok(None);
    };
    if record.verify().is_err()
        || record.pubkey != relay.keys.public_key()
        || record.kind.as_u16() != KIND_INTEGRATION_RECORD as u16
        || value(&record, "d") != Some(coordinate)
    {
        bail!("invalid integration record");
    }
    Ok(Some(record))
}

async fn save(relay: &Relay, channel: Uuid, coordinate: &str, plan: &Plan) -> Result<()> {
    validate(relay, channel, plan)?;
    let event = record(relay, channel, coordinate, plan, Timestamp::now())?;
    if relay.submit(&event).await? {
        return Ok(());
    }
    // The relay keeps the newest record per coordinate, breaks same-second
    // ties by lowest ID, and reports a losing write as a duplicate. The plan
    // or route being saved now must win, so supersede the current head.
    let Some(current) = head(relay, coordinate).await? else {
        bail!("integration record is missing after its write");
    };
    if current.id == event.id {
        return Ok(());
    }
    let next = Timestamp::from(current.created_at.as_secs().saturating_add(1));
    if !relay
        .submit(&record(relay, channel, coordinate, plan, next)?)
        .await?
    {
        bail!("integration record did not replace its head");
    }
    Ok(())
}

fn record(
    relay: &Relay,
    channel: Uuid,
    coordinate: &str,
    plan: &Plan,
    created_at: Timestamp,
) -> Result<Event> {
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
    Ok(
        EventBuilder::new(Kind::Custom(KIND_INTEGRATION_RECORD as u16), body)
            .tags(tags)
            .custom_created_at(created_at)
            .sign_with_keys(&relay.keys)?,
    )
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
    let delivery = format!("delivery:{scope}:{}", item.delivery);
    let fingerprint = format!("fingerprint:{scope}:{body_hash}");
    let a = read(relay, item.channel, &delivery).await?;
    // A retried batch replays its completed occurrences first. Finish each in
    // two reads so later occurrences still fit within the delivery deadline.
    // The reply is submitted only after its anchor and both plans exist.
    if let Some(done) = &a {
        if done.payload_sha256.as_deref() == Some(body_hash)
            && exists(relay, item.channel, &done.event).await?
        {
            return Ok(());
        }
    }
    let b = read(relay, item.channel, &fingerprint).await?;
    for plan in a.iter().chain(b.iter()) {
        if plan.payload_sha256.as_deref() != Some(body_hash) {
            bail!("delivery identity conflicts with saved payload");
        }
    }
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
    if !exists(relay, item.channel, &root.event).await? {
        if stale(&root) {
            freshen(relay, &mut root)?;
        }
        save(relay, item.channel, &root_coord, &root).await?;
        relay.submit(&root.event).await?;
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
    let anchor = root.event.id.to_hex();
    if !accepted && (stale(&plan) || value(&plan.event, "e") != Some(anchor.as_str())) {
        // Only an authoritative successful empty lookup permits changing IDs.
        if stale(&plan) {
            freshen(relay, &mut plan)?;
        }
        let mut tags = plan.event.tags.clone().to_vec();
        tags.retain(|tag| tag.as_slice().first().map(String::as_str) != Some("e"));
        tags.push(Tag::parse(["e", &root.event.id.to_hex(), "", "root"])?);
        tags.push(Tag::parse(["e", &root.event.id.to_hex(), "", "reply"])?);
        plan.event = EventBuilder::new(Kind::Custom(9), plan.event.content.clone())
            .tags(tags)
            .custom_created_at(plan.event.created_at)
            .sign_with_keys(&relay.keys)?;
    }
    save(relay, item.channel, &delivery, &plan).await?;
    save(relay, item.channel, &fingerprint, &plan).await?;
    if !accepted {
        relay.submit(&plan.event).await?;
    }
    Ok(())
}

/// The saved route of a known issue: a channel, or cleared after the issue
/// left every mapped scope.
pub(crate) enum LinearRoute {
    Mapped(Uuid),
    Unmapped,
}

/// The routing decision of one Linear issue version: the channel for later
/// comments without routing metadata, or none after the issue left every
/// mapped scope. Its version is the payload's signed `updatedAt`.
pub(crate) use buzz_core::integration::Route;

fn route_coordinate(issue: &str) -> String {
    format!("route:linear:{}", hex::encode(Sha256::digest(issue)))
}

fn linear_channels(config: &Config) -> impl Iterator<Item = Uuid> + '_ {
    config
        .linear_projects
        .values()
        .chain(config.linear_teams.values())
        .chain(config.linear_webhooks.values())
        .copied()
}

/// Marks the route channel. A read that names the route channel returns the
/// marker exactly when it can return the channel's routes, so a route missing
/// from a read with the marker is missing, not hidden.
const ROUTE_MARKER: &str = "route-channel:linear";

/// The issue's saved route in the route channel, and whether the same read
/// returned the route channel marker. Read errors propagate.
async fn route_head(relay: &Relay, routes: Uuid, issue: &str) -> Result<(Option<Route>, bool)> {
    let coordinate = route_coordinate(issue);
    let records = relay
        .query(json!({"kinds":[KIND_INTEGRATION_RECORD],"authors":[relay.keys.public_key().to_hex()],"#d":[coordinate, ROUTE_MARKER],"#h":[routes.to_string()],"limit":2}))
        .await?;
    let (mut route, mut marked) = (None, false);
    for record in &records {
        if record.verify().is_err()
            || record.pubkey != relay.keys.public_key()
            || record.kind.as_u16() != KIND_INTEGRATION_RECORD as u16
        {
            bail!("invalid integration record");
        }
        match value(record, "d") {
            Some(d) if d == coordinate && route.is_none() => {
                route = Some(integration::route(&record.tags).map_err(anyhow::Error::msg)?);
            }
            Some(ROUTE_MARKER) if !marked => marked = true,
            _ => bail!("ambiguous integration record"),
        }
    }
    Ok((route, marked))
}

/// The issue's saved route, or `None` when it has none. A route channel that
/// the publisher cannot read can hide a saved route, so the route channel
/// marker must be in the same read; it is written once when missing.
async fn known_route(relay: &Relay, routes: Uuid, issue: &str) -> Result<Option<Route>> {
    for attempt in 0..2 {
        match route_head(relay, routes, issue).await? {
            (Some(route), _) => return Ok(Some(route)),
            (None, true) => return Ok(None),
            (None, false) if attempt == 0 => {
                // The relay stores this only from a writer of the channel.
                let marker = EventBuilder::new(Kind::Custom(KIND_INTEGRATION_RECORD as u16), "")
                    .tags([tag("d", ROUTE_MARKER)?, tag("h", &routes.to_string())?])
                    .sign_with_keys(&relay.keys)?;
                relay.submit(&marker).await?;
            }
            (None, false) => {}
        }
    }
    bail!("Linear route channel is unreadable")
}

/// Save `next` as the route of `issue` unless the saved route is greater.
/// Linear can deliver and retry issue versions in any order. Comparing the
/// signed versions, not write times, lets every order converge on the newest
/// version's decision. An older retry or replay therefore cannot restore a
/// cleared route or clear a restored one.
///
/// A clear is saved even without a previous route, so a late older mapped
/// version cannot create one. The first versioned route also replaces a route
/// saved by a release before version ordering. Records live in the configured
/// route channel, which no mapped channel membership affects.
pub(crate) async fn route_linear_issue(
    relay: &Relay,
    config: &Config,
    issue: &str,
    next: Route,
) -> Result<()> {
    let Some(routes) = config.linear_route_channel else {
        // Without a Linear mapping no route can be saved or read.
        return Ok(());
    };
    if route_head(relay, routes, issue)
        .await?
        .0
        .is_some_and(|saved| saved >= next)
    {
        return Ok(());
    }
    let mut tags = vec![
        tag("d", &route_coordinate(issue))?,
        tag("h", &routes.to_string())?,
        tag(integration::TAG_ROUTE_VERSION, &next.version.to_string())?,
    ];
    if let Some(channel) = next.channel {
        tags.push(tag(integration::TAG_ROUTE, &channel.to_string())?);
    }
    let record = EventBuilder::new(Kind::Custom(KIND_INTEGRATION_RECORD as u16), "")
        .tags(tags)
        .sign_with_keys(&relay.keys)?;
    // The relay keeps the greatest route under its coordinate lock and
    // reports any other write as a duplicate. An earlier write whose outcome
    // is unknown, even from before a restart, can therefore land late without
    // replacing a newer route.
    if relay.submit(&record).await? {
        return Ok(());
    }
    // Confirm that the saved route is not older. A relay without route
    // ordering, or an unreadable route channel, fails here for retry.
    match route_head(relay, routes, issue).await?.0 {
        Some(saved) if saved >= next => Ok(()),
        _ => bail!("issue route did not reach its version"),
    }
}

/// The route that releases before version ordering saved for `issue`: its
/// channel, or `None` after the issue was retired. These records carry no
/// issue version, so they apply only until the issue has a versioned route.
async fn released_route(relay: &Relay, issue: &str) -> Result<Option<Option<Uuid>>> {
    let coordinate = format!("route:linear:issue:{issue}");
    let Some(record) = head(relay, &coordinate).await? else {
        return Ok(None);
    };
    let channel = value(&record, "h")
        .and_then(|id| Uuid::parse_str(id).ok())
        .context("invalid released issue route channel")?;
    let plan = decode(relay, channel, &coordinate, &record)?;
    let retired = value(&plan.event, "ext-route-state") == Some("unmapped");
    Ok(Some((!retired).then_some(channel)))
}

/// Resolve comments whose signed payload omits project/team information.
/// Explicit webhook mappings take precedence over this saved route.
pub(crate) async fn linear_channel(
    relay: &Relay,
    issue: &str,
    config: &Config,
) -> Result<Option<LinearRoute>> {
    let Some(routes) = config.linear_route_channel else {
        return Ok(None);
    };
    // A versioned route outranks a released one, whichever was written later,
    // so a released record cannot undo an ordered decision.
    let channel = match known_route(relay, routes, issue).await? {
        Some(route) => route.channel,
        None => match released_route(relay, issue).await? {
            Some(channel) => channel,
            None => return Ok(None),
        },
    };
    let Some(channel) = channel else {
        return Ok(Some(LinearRoute::Unmapped));
    };
    if !linear_channels(config).any(|known| known == channel) {
        bail!("Linear issue route is outside configured channels");
    }
    Ok(Some(LinearRoute::Mapped(channel)))
}
