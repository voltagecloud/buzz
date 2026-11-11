//! Owner-approved exports with current authority and durable recovery metadata.

use super::{
    authority, client,
    server::Config,
    snapshot_journal::{Journal, Record},
    snapshot_run::{self, Step},
};
use crate::relay::RestClient;
use anyhow::{bail, ensure, Result};
use buzz_sdk::sandbox_attach::{
    self as attach,
    snapshot::{self, Failure, Grant},
};
use buzz_ws_client::{NostrWsConnection, OkResponse, RelayMessage, WsClientError};
use nostr::{Event, EventBuilder, Keys, Timestamp};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::{timeout, timeout_at, Instant};

const PERIOD: Duration = Duration::from_secs(5);
const WORK_BYTES: u64 = 512 * 1024 * 1024;
const HELPER: &str = "/usr/local/bin/buzz-agent-snapshot";

fn deadline(grant: &Grant, checked: Instant) -> Instant {
    let remaining = (std::time::UNIX_EPOCH + Duration::from_secs(grant.expires_at()))
        .duration_since(std::time::SystemTime::now())
        .unwrap_or(Duration::ZERO);
    (checked + PERIOD + authority::AUTH_TIMEOUT).min(Instant::now() + remaining)
}
// Reading drives the shared client's Ping/Pong control path. There is no
// background reader, so idle scans/uploads must keep polling this socket.
fn control(message: std::result::Result<RelayMessage, WsClientError>) -> Result<()> {
    match message {
        Ok(RelayMessage::Auth { .. }) => bail!("snapshot relay authentication changed"),
        Ok(_) | Err(WsClientError::Timeout) => Ok(()),
        Err(_) => bail!("snapshot relay transport closed"),
    }
}

async fn before_claim(
    config: &Config,
    rest: &RestClient,
    connection: &mut NostrWsConnection,
    grant: &Grant,
    shutdown: &mut watch::Receiver<()>,
) -> Result<()> {
    // Leave the caller's single shutdown notification unseen when this helper
    // returns an error and the grant loop continues.
    let mut shutdown = shutdown.clone();
    let current = authority::refresh_snapshot(rest, config.owner, grant);
    tokio::pin!(current);
    loop {
        tokio::select! {
            biased;
            _=shutdown.changed()=>bail!("snapshot service is shutting down"),
            result=&mut current=>return result,
            message=connection.next_event(PERIOD)=>control(message)?,
        }
    }
}

async fn publish(connection: &mut NostrWsConnection, event: &Event) -> Result<OkResponse> {
    timeout(PERIOD, async {
        timeout(
            Duration::from_secs(1),
            connection.send_raw(&json!(["EVENT", event])),
        )
        .await??;
        for _ in 0..256 {
            if let RelayMessage::Ok(receipt) = connection.next_event(PERIOD).await? {
                if receipt.event_id == event.id.to_hex() {
                    return Ok(receipt);
                }
            }
        }
        bail!("snapshot receipt budget exhausted")
    })
    .await?
}
fn accepted(receipt: &OkResponse, event: &Event) -> Result<()> {
    ensure!(
        receipt.accepted && receipt.event_id == event.id.to_hex(),
        "snapshot metadata refused"
    );
    Ok(())
}
fn first_claim(receipt: &OkResponse, event: &Event) -> Result<()> {
    ensure!(
        receipt.accepted
            && receipt.event_id == event.id.to_hex()
            && receipt.message == attach::CLAIM_OK_MESSAGE,
        "snapshot atomic first claim was not confirmed"
    );
    Ok(())
}
async fn metadata(
    connection: &mut NostrWsConnection,
    rest: &RestClient,
    event: &Event,
) -> Result<()> {
    accepted(&publish(connection, event).await?, event)?;
    authority::confirm_snapshot_metadata(rest, event).await
}
async fn renewed_metadata(rest: &RestClient, keys: &Keys, event: &Event) -> Result<(Event, bool)> {
    let head = authority::snapshot_metadata_head(rest, event).await?;
    if head.as_ref() == Some(event) {
        return Ok((event.clone(), true));
    }
    ensure!(
        head.is_none(),
        "snapshot audit coordinate changed unexpectedly"
    );
    if Timestamp::now()
        .as_secs()
        .abs_diff(event.created_at.as_secs())
        < 840
    {
        return Ok((event.clone(), false));
    }
    // Fresh envelopes preserve only identical historical keyless metadata.
    // Claims and recipient-encrypted Ready receipts never enter this path.
    ensure!(
        event.pubkey == keys.public_key()
            && event.tags.iter().any(|tag| {
                let values = tag.as_slice();
                values.first().is_some_and(|key| key == "d")
                    && values.get(1).is_some_and(|value| {
                        value.starts_with("snapshot:")
                            && [":upload", ":upload-outcome", ":ended"]
                                .iter()
                                .any(|suffix| value.ends_with(suffix))
                    })
            }),
        "snapshot replay must remain keyless metadata"
    );
    Ok((
        EventBuilder::new(event.kind, event.content.clone())
            .tags(event.tags.clone().to_vec())
            .sign_with_keys(keys)?,
        false,
    ))
}
// One keyless record per tick bounds shutdown and retry work independently of backlog.
async fn outbox(
    connection: &mut NostrWsConnection,
    rest: &RestClient,
    journal: &Journal,
    keys: &Keys,
) -> Result<()> {
    for mut record in journal.records()? {
        for index in 0..record.metadata.len() {
            let event = record.metadata[index].clone();
            if record.sent.contains(&event.id) {
                continue;
            }
            let (event, stored) = renewed_metadata(rest, keys, &event).await?;
            if event.id != record.metadata[index].id {
                record.metadata[index] = event.clone();
                journal.save(&record)?;
            }
            if !stored {
                metadata(connection, rest, &event).await?;
            }
            record.sent.push(event.id);
            journal.save(&record)?;
            return Ok(());
        }
        if let Some(ended) = record.ended.as_ref().filter(|_| !record.ended_sent) {
            let (event, stored) = renewed_metadata(rest, keys, ended).await?;
            record.ended = Some(event.clone());
            journal.save(&record)?;
            if !stored {
                metadata(connection, rest, &event).await?;
            }
            record.ended_sent = true;
            journal.save(&record)?;
            return Ok(());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn export(
    config: &Config,
    rest: &RestClient,
    connection: &mut NostrWsConnection,
    journal: &Journal,
    grant: &Grant,
    record: &mut Record,
    helper: PathBuf,
    mut shutdown: watch::Receiver<()>,
) -> Result<()> {
    let checked = Instant::now();
    authority::refresh_snapshot(rest, config.owner, grant).await?;
    ensure!(
        !shutdown.has_changed().unwrap_or(true),
        "snapshot service is shutting down"
    );
    let lease = deadline(grant, checked);
    ensure!(
        lease.saturating_duration_since(Instant::now()) >= client::READY_TIMEOUT,
        "snapshot admission lease cannot cover worker readiness"
    );
    journal.prepare(record)?;
    let directory = journal.job(grant.id());
    let (allowed_tx, allowed) = watch::channel(true);
    let (transport_lost, mut transport_failure) = watch::channel(());
    let delivery_allowed = allowed.clone();
    let monitor_rest = rest.clone();
    let monitor_grant = grant.clone();
    let owner = config.owner;
    let mut monitor_shutdown = shutdown.clone();
    // This independent monitor keeps expiry/revocation live while metadata I/O
    // waits. Its sender loss also invalidates the runner's authority lease.
    let monitor = tokio::spawn(async move {
        let mut lease = lease;
        let mut next_check = Instant::now() + PERIOD;
        loop {
            tokio::select! {
                biased;
                _=monitor_shutdown.changed()=>break,
                _=transport_failure.changed()=>break,
                _=tokio::time::sleep_until(lease)=>break,
                _=tokio::time::sleep_until(next_check)=>{},
            }
            let checked = Instant::now();
            next_check = checked + PERIOD;
            let result = tokio::select! {
                biased;
                _=monitor_shutdown.changed()=>break,
                _=transport_failure.changed()=>break,
                _=tokio::time::sleep_until(lease)=>break,
                result=authority::refresh_snapshot(&monitor_rest,owner,&monitor_grant)=>result,
            };
            if result.is_err() {
                break;
            }
            lease = deadline(&monitor_grant, checked);
        }
        let _ = allowed_tx.send(false);
    });
    let (steps, mut updates) = mpsc::channel(1);
    let accepted_event = record.accepted.clone();
    let runner = snapshot_run::run(
        config,
        rest,
        grant,
        &accepted_event,
        &directory,
        &helper,
        WORK_BYTES,
        allowed,
        steps,
    );
    tokio::pin!(runner);
    let mut stopped = false;
    let mut readable = true;
    let result = loop {
        tokio::select! {
            biased;
            _=shutdown.changed(),if !stopped=>{stopped=true;}
            result=&mut runner=>break result,
            message=connection.next_event(PERIOD),if readable=>{
                if control(message).is_err() {
                    readable=false;stopped=true;let _=transport_lost.send(());
                }
            }
            step=updates.recv()=>{
                let Some(step)=step else {continue;};
                match step {
                    Step::Metadata {event,confirmed}=>{
                        let result=async {
                            ensure!(record.metadata.len()<2,"snapshot metadata budget exceeded");
                            let parsed=snapshot::upload::Upload::parse(&event,config.scope,config.owner)?;
                            ensure!(parsed.grant().id()==grant.id() && parsed.claim()==record.accepted.id,
                                "snapshot metadata attempt mismatch");
                            record.metadata.push(event.clone());journal.save(record)?;
                            metadata(connection,rest,&event).await?;record.sent.push(event.id);journal.save(record)
                        }.await;
                        let _=confirmed.send(result);
                    }
                    Step::Ready {event,confirmed}=>{
                        // A key-bearing receipt is never persisted in the replay outbox.
                        let delivery=async {
                            ensure!(!stopped && *delivery_allowed.borrow() && delivery_allowed.has_changed().is_ok(),"snapshot authority ended");
                            let checked=Instant::now();authority::refresh_snapshot(rest,config.owner,grant).await?;
                            let gate=deadline(grant,checked);
                            ensure!(Instant::now()<gate,"snapshot authority lease expired");
                            let receipt=timeout_at(gate,publish(connection,&event)).await??;
                            accepted(&receipt,&event)
                        };
                        let mut canceled=delivery_allowed.clone();
                        let result=tokio::select! {
                            biased;
                            _=shutdown.changed()=>Err(anyhow::anyhow!("snapshot shutdown before delivery")),
                            _=canceled.wait_for(|value|!*value)=>Err(anyhow::anyhow!("snapshot authority ended before delivery")),
                            result=delivery=>result,
                        };
                        if result.is_err(){stopped=true;}
                        let _=confirmed.send(result);
                    }
                }
            }

        }
    };
    monitor.abort();
    if result
        .as_ref()
        .is_err_and(|error| error.is::<client::ContainmentError>())
    {
        return result.map(|_| ());
    }
    let (bytes, reason) = match result {
        Ok(done) => (done.bytes, done.reason),
        Err(_) => (
            0,
            Some(if stopped {
                Failure::Authority
            } else {
                Failure::Archive
            }),
        ),
    };
    journal.finish(&config.keys, record, bytes, reason)?;
    journal.cleanup(record)?;
    Ok(())
}

async fn run(
    config: &Config,
    rest: &RestClient,
    helper: PathBuf,
    mut shutdown: watch::Receiver<()>,
) -> Result<()> {
    let journal = Journal::open(config.journal.clone(), config.scope, config.owner)?;
    journal.recover(&config.keys)?;
    let mut connection = tokio::select! {
        _=shutdown.changed()=>return Ok(()),
        result=timeout(Duration::from_secs(45),NostrWsConnection::connect_authenticated(&config.relay,&config.keys,config.auth.as_ref()))=>result??,
    };
    let mut poll = tokio::time::interval(PERIOD);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _=shutdown.changed()=>return Ok(()),
            _=poll.tick()=>{},
            message=connection.next_event(PERIOD)=>{control(message)?;continue;}
        }
        journal.recover(&config.keys)?;
        if outbox(&mut connection, rest, &journal, &config.keys)
            .await
            .is_err()
        {
            bail!("snapshot audit retained for retry");
        }
        let events = authority::snapshot_grants(rest, config.owner, config.scope).await?;
        let records = journal.records()?;
        for event in events {
            let Ok(grant) = Grant::parse(&event, config.scope, Timestamp::now().as_secs()) else {
                continue;
            };
            if records.iter().any(|record| record.grant.id == grant.id()) {
                continue;
            }
            if shutdown.has_changed().unwrap_or(true) {
                return Ok(());
            }
            if before_claim(config, rest, &mut connection, &grant, &mut shutdown)
                .await
                .is_err()
            {
                continue;
            }
            let mut record = journal.reserve(&config.keys, &grant)?;
            let claim = publish(&mut connection, &record.accepted).await?;
            first_claim(&claim, &record.accepted)?;
            record.claimed = true;
            journal.save(&record)?;
            // A failed admission still durably ends this owned claim; never retry it.
            if let Err(error) = export(
                config,
                rest,
                &mut connection,
                &journal,
                &grant,
                &mut record,
                helper.clone(),
                shutdown.clone(),
            )
            .await
            {
                if error.is::<client::ContainmentError>() {
                    return Err(error);
                }
                journal.finish(&config.keys, &mut record, 0, Some(Failure::Authority))?;
                journal.cleanup(&mut record)?;
            }
            if shutdown.has_changed().unwrap_or(true) {
                return Ok(());
            }
            break;
        }
    }
}

/// Production reaches this engine only through the owner-gated WSS factory.
/// Tests may supply a local relay and an immutable fixture helper privately.
pub(super) async fn serve(
    config: Config,
    rest: RestClient,
    helper: PathBuf,
    mut shutdown: watch::Receiver<()>,
) -> Result<()> {
    for attempt in 0..5 {
        match run(&config, &rest, helper.clone(), shutdown.clone()).await {
            Ok(()) => return Ok(()),
            Err(error) if error.is::<client::ContainmentError>() => return Err(error),
            Err(_) => tracing::warn!("snapshot service stopped with durable recovery state"),
        }
        tokio::select! {_=shutdown.changed()=>return Ok(()),_=tokio::time::sleep(Duration::from_secs(1<<attempt))=>{}}
    }
    bail!("snapshot service retries exhausted")
}

pub(super) fn helper() -> PathBuf {
    PathBuf::from(HELPER)
}

#[cfg(test)]
mod tests;
