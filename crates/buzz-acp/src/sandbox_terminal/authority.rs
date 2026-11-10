//! Current owner approval and independent channel/delegation authority.

use anyhow::{ensure, Context, Result};
use buzz_sdk::sandbox_attach::{Authority, Grant, Pending, Scope};
use nostr::{Event, EventId, PublicKey, Timestamp};
use serde_json::{json, Value};

use crate::relay::RestClient;

pub(super) const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub(super) struct Snapshot {
    owner: PublicKey,
    relay: PublicKey,
    roster: Event,
    delegations: Vec<Event>,
    profiles: Vec<Event>,
}

impl Snapshot {
    fn authority(&self) -> Authority<'_> {
        Authority {
            owner: self.owner,
            relay: self.relay,
            roster: &self.roster,
            delegations: &self.delegations,
            profiles: &self.profiles,
        }
    }

    pub(super) fn authors(&self, scope: Scope) -> Vec<PublicKey> {
        let mut authors = vec![self.owner];
        authors.extend(
            buzz_sdk::gateway::resolve(
                self.owner,
                &self.delegations,
                &self.profiles,
                &[self.owner, scope.agent],
                Timestamp::now().as_secs(),
            )
            .into_iter()
            .filter(|live| {
                buzz_sdk::gateway::channel_member(
                    &self.roster,
                    self.relay,
                    scope.channel,
                    live.gateway(),
                )
            })
            .map(|live| live.gateway()),
        );
        authors
    }
}

async fn query(rest: &RestClient, mut filter: Value, limit: usize) -> Result<Vec<Event>> {
    filter["consistency"] = json!("strong");
    let events: Vec<Event> = serde_json::from_value(rest.query_raw(&[filter]).await?)?;
    ensure!(
        events.len() <= limit,
        "terminal authority response exceeds bound"
    );
    Ok(events)
}

async fn read(rest: &RestClient, owner: PublicKey, scope: Scope) -> Result<Snapshot> {
    let relay = rest
        .relay_self()
        .await?
        .and_then(|value| PublicKey::from_hex(&value).ok())
        .context("terminal relay identity unavailable")?;
    let delegations = query(
        rest,
        json!({"kinds":[30181], "authors":[owner],
        "limit":buzz_sdk::gateway::LOOKUP_LIMIT}),
        buzz_sdk::gateway::LOOKUP_LIMIT,
    )
    .await?;
    let candidates: Vec<_> =
        buzz_sdk::gateway::live_heads(owner, &delegations, Timestamp::now().as_secs())
            .into_iter()
            .map(|live| live.gateway())
            .collect();
    let profiles = if candidates.is_empty() {
        Vec::new()
    } else {
        query(
            rest,
            json!({"kinds":[0], "authors":candidates, "limit":candidates.len()}),
            candidates.len(),
        )
        .await?
    };
    let mut rosters = query(
        rest,
        json!({"kinds":[39002], "authors":[relay], "#d":[scope.channel], "limit":1}),
        1,
    )
    .await?;
    let roster = rosters
        .pop()
        .context("terminal channel membership unavailable")?;
    ensure!(
        [owner, scope.agent]
            .into_iter()
            .all(|key| buzz_sdk::gateway::channel_member(&roster, relay, scope.channel, key)),
        "terminal owner or agent no longer has posting membership"
    );
    Ok(Snapshot {
        owner,
        relay,
        roster,
        delegations,
        profiles,
    })
}

pub(super) async fn snapshot(
    rest: &RestClient,
    owner: PublicKey,
    scope: Scope,
) -> Result<Snapshot> {
    tokio::time::timeout(AUTH_TIMEOUT, read(rest, owner, scope)).await?
}

async fn current(rest: &RestClient, snapshot: &Snapshot, grant: &Grant) -> Result<()> {
    let coordinate = |author, phase| {
        json!({"kinds":[30624], "authors":[author],
        "#d":[format!("terminal:{}:{phase}", grant.session())], "limit":1})
    };
    let (approvals, pending) = tokio::try_join!(
        query(rest, coordinate(snapshot.owner, "grant"), 1),
        query(rest, coordinate(grant.requester(), "request"), 1),
    )?;
    ensure!(
        approvals
            .as_slice()
            .first()
            .is_some_and(|event| event.id == grant.id() && event.verify().is_ok()),
        "terminal approval was revoked or replaced"
    );
    let pending = Pending::parse(
        pending.first().context("terminal request was revoked")?,
        Timestamp::now().as_secs(),
    )?;
    ensure!(
        grant.authorized(&pending, snapshot.authority(), Timestamp::now().as_secs()),
        "terminal approval is not currently authorized"
    );
    Ok(())
}

pub(super) async fn approve(
    rest: &RestClient,
    owner: PublicKey,
    scope: Scope,
    id: EventId,
    requester: PublicKey,
) -> Result<Grant> {
    tokio::time::timeout(AUTH_TIMEOUT, async {
        let snapshot = read(rest, owner, scope).await?;
        let events = query(rest, json!({"kinds":[30624], "ids":[id], "limit":1}), 1).await?;
        let event = events.first().context("terminal approval unavailable")?;
        ensure!(
            event.id == id && event.pubkey == owner,
            "terminal approval signer mismatch"
        );
        let grant = Grant::parse(event, scope, Timestamp::now().as_secs())?;
        ensure!(
            grant.requester() == requester,
            "terminal requester mismatch"
        );
        current(rest, &snapshot, &grant).await?;
        Ok(grant)
    })
    .await?
}

pub(super) async fn refresh(
    rest: &RestClient,
    owner: PublicKey,
    grant: &Grant,
) -> Result<Snapshot> {
    tokio::time::timeout(AUTH_TIMEOUT, async {
        let snapshot = read(rest, owner, grant.scope()).await?;
        current(rest, &snapshot, grant).await?;
        Ok(snapshot)
    })
    .await?
}
