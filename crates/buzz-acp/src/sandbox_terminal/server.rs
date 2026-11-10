//! One owner-approved PTY, with independent live authority and reliable relay I/O.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use buzz_sdk::sandbox_attach::{self as attach, Flow, Frame, Grant, Packet, Rate, Scope};
use buzz_ws_client::{NostrWsConnection, OkResponse, RelayMessage};
use nostr::{Event, EventId, Keys, PublicKey, Tag, Timestamp};
use serde_json::json;
use tokio::sync::watch;
use tokio::time::{timeout, Instant};

use super::{
    authority, client,
    journal::{Journal, Record},
};
use crate::config::{BoundSession, RespondTo};
use crate::relay::RestClient;

const REFRESH: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(1);
const IDLE: Duration = Duration::from_secs(300);
const DRAIN: Duration = Duration::from_secs(6);
const SUBSCRIPTION: &str = "sandbox-terminal";

pub(super) struct Config {
    pub(super) relay: String,
    pub(super) keys: Keys,
    pub(super) auth: Option<Tag>,
    pub(super) owner: PublicKey,
    pub(super) scope: Scope,
    pub(super) workspace: PathBuf,
    pub(super) executable: PathBuf,
    pub(super) journal: PathBuf,
}

impl Config {
    pub(super) fn from_harness(config: &crate::config::Config) -> Result<Option<Self>> {
        let Some(BoundSession::Thread {
            channel_id,
            thread_root,
        }) = &config.bound_session
        else {
            return Ok(None);
        };
        if config.respond_to != RespondTo::Owner
            || url::Url::parse(&config.relay_url)?.scheme() != "wss"
        {
            return Ok(None);
        }
        let Some(owner) = crate::resolve_agent_owner(config) else {
            return Ok(None);
        };
        let owner = PublicKey::from_hex(&owner)?;
        ensure!(
            owner != config.keys.public_key(),
            "terminal owner must be distinct from agent"
        );
        let scope = Scope {
            channel: *channel_id,
            thread: EventId::from_hex(thread_root)?,
            agent: config.keys.public_key(),
        };
        let home = std::env::var_os("HOME").context("terminal journal requires HOME")?;
        let journal = PathBuf::from(home)
            .join(".local/state/buzz/terminal")
            .join(scope.agent.to_hex())
            .join(scope.thread.to_hex());
        let auth = std::env::var("BUZZ_AUTH_TAG")
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| buzz_sdk::nip_oa::parse_auth_tag(&value))
            .transpose()?;
        Ok(Some(Self {
            relay: config.relay_url.clone(),
            keys: config.keys.clone(),
            auth,
            owner,
            scope,
            workspace: std::env::current_dir()?,
            executable: std::env::current_exe()?,
            journal,
        }))
    }
}

struct Session {
    grant: Grant,
    record: Record,
    pty: client::Pty,
    flow: Flow,
    last_sent: u64,
    acknowledged: u64,
    idle: Instant,
    heartbeat: Instant,
    exited: Option<Instant>,
    close_sent: bool,
    output: Option<Vec<u8>>,
    authority_deadline: Instant,
}

struct RefreshResult {
    generation: Option<EventId>,
    checked_at: Instant,
    verdict: Result<authority::Snapshot>,
}

fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn authority_deadline(grant: &Grant, checked_at: Instant) -> Instant {
    let expiry = std::time::UNIX_EPOCH + Duration::from_secs(grant.expires_at());
    let remaining = expiry
        .duration_since(std::time::SystemTime::now())
        .unwrap_or(Duration::ZERO);
    (checked_at + REFRESH + authority::AUTH_TIMEOUT).min(Instant::now() + remaining)
}

async fn raw(connection: &mut NostrWsConnection, value: serde_json::Value) -> Result<()> {
    timeout(IO_TIMEOUT, connection.send_raw(&value)).await??;
    Ok(())
}

/// Read the real per-write receipt without the shared client's unbounded
/// wait-for-OK buffer. No worker is running while metadata is published.
async fn publish(connection: &mut NostrWsConnection, event: &Event) -> Result<OkResponse> {
    timeout(REFRESH, async {
        raw(connection, json!(["EVENT", event])).await?;
        for _ in 0..256 {
            if let RelayMessage::Ok(receipt) = connection.next_event(REFRESH).await? {
                if receipt.event_id == event.id.to_hex() {
                    return Ok(receipt);
                }
            }
        }
        bail!("terminal receipt message budget exhausted")
    })
    .await?
}

fn first_claim(receipt: &OkResponse, event: &Event) -> Result<()> {
    ensure!(
        receipt.accepted
            && receipt.event_id == event.id.to_hex()
            && receipt.message == attach::CLAIM_OK_MESSAGE,
        "terminal first claim was not confirmed"
    );
    Ok(())
}

async fn subscribe(
    connection: &mut NostrWsConnection,
    config: &Config,
    snapshot: &authority::Snapshot,
) -> Result<()> {
    raw(
        connection,
        json!(["REQ", SUBSCRIPTION, {"kinds":[24203],
        "authors":snapshot.authors(config.scope), "#p":[config.scope.agent],
        "#h":[config.scope.channel], "#e":[config.scope.thread], "limit":0}]),
    )
    .await
}

async fn send(
    connection: &mut NostrWsConnection,
    config: &Config,
    session: &Session,
    frame: &Frame,
) -> Result<()> {
    let event = attach::encrypt(&config.keys, session.grant.requester(), config.scope, frame)?;
    tokio::time::timeout_at(
        session.authority_deadline,
        raw(connection, json!(["EVENT", event])),
    )
    .await?
}

async fn queue(
    connection: &mut NostrWsConnection,
    config: &Config,
    session: &mut Session,
    packet: Packet,
    ms: u64,
) -> Result<()> {
    let frame = session.flow.send(packet, ms)?;
    session.last_sent = frame.seq;
    send(connection, config, session, &frame).await
}

async fn outbox(connection: &mut NostrWsConnection, journal: &Journal) -> Result<()> {
    // One event per idle tick bounds both work and receipt-wait time. Retain
    // every failure in the journal until the exact stored event is accepted.
    for mut record in journal.records()? {
        if record.ended_sent {
            continue;
        }
        if let Some(event) = &record.ended {
            let receipt = publish(connection, event).await?;
            ensure!(
                receipt.accepted && receipt.event_id == event.id.to_hex(),
                "terminal end audit refused"
            );
            record.ended_sent = true;
            journal.save(&record)?;
            break;
        }
    }
    Ok(())
}

async fn open(
    connection: &mut NostrWsConnection,
    config: &Config,
    rest: &RestClient,
    journal: &Journal,
    event: &Event,
    outgoing: &mut Rate,
    start: Instant,
) -> Result<Session> {
    // Bound ciphertext before decryption. The second typed decryption binds
    // the discovered approval ID to the complete signed envelope.
    ensure!(
        event.content.len() <= 32 * 1024 && event.verify().is_ok(),
        "invalid terminal open"
    );
    let discovered: Frame = buzz_core::observer::decrypt_observer_payload(&config.keys, event)?;
    let frame = attach::decrypt(
        &config.keys,
        event.pubkey,
        config.scope,
        discovered.grant,
        event,
        Timestamp::now().as_secs(),
    )?;
    let Packet::Open { rows, cols } = frame.packet else {
        bail!("terminal requires Open")
    };
    ensure!(
        frame.seq == 1 && frame.ack == 0,
        "terminal Open must start its transport"
    );
    let checked_at = Instant::now();
    let grant =
        authority::approve(rest, config.owner, config.scope, frame.grant, event.pubkey).await?;
    let mut record = journal.reserve(&config.keys, &grant)?;
    let receipt = publish(connection, &record.accepted).await?;
    first_claim(&receipt, &record.accepted)?;
    record.claimed = true;
    journal.save(&record)?;
    let deadline = authority_deadline(&grant, checked_at);
    if Instant::now() + client::READY_TIMEOUT >= deadline {
        journal.finish(&config.keys, &mut record)?;
        bail!("terminal admission has insufficient live authority for readiness");
    }
    let pty = match client::Pty::open(
        &config.executable,
        &config.workspace,
        grant.expires_at(),
        rows,
        cols,
    )
    .await
    {
        Ok(pty) => pty,
        // The durable owned acceptance already permits recovery of its audit.
        // A second journal failure must never downgrade lost containment.
        Err(error) if error.is::<client::ContainmentError>() => return Err(error),
        Err(error) => {
            journal.finish(&config.keys, &mut record)?;
            return Err(error);
        }
    };
    let mut flow = Flow::new(grant.id());
    flow.receive(Frame {
        grant: frame.grant,
        seq: 1,
        ack: 0,
        packet: Packet::Open { rows, cols },
    })?;
    let mut session = Session {
        grant,
        record,
        pty,
        flow,
        last_sent: 0,
        acknowledged: 0,
        idle: Instant::now(),
        heartbeat: Instant::now(),
        exited: None,
        close_sent: false,
        output: None,
        authority_deadline: deadline,
    };
    // An ACK cannot precede the worker's Ready receipt. Errors after spawn
    // still confirm containment and durably finish the owned acceptance.
    let ready = async {
        ensure!(
            outgoing.allow(elapsed(start)),
            "terminal send budget exhausted"
        );
        send(
            connection,
            config,
            &session,
            &session.flow.acknowledgement(),
        )
        .await
    }
    .await;
    if let Err(error) = ready {
        session.pty.close().await?;
        journal.finish(&config.keys, &mut session.record)?;
        return Err(error);
    }
    Ok(session)
}

async fn finish(config: &Config, journal: &Journal, active: &mut Option<Session>) -> Result<()> {
    if let Some(mut session) = active.take() {
        session.pty.close().await?;
        journal.finish(&config.keys, &mut session.record)?;
    }
    Ok(())
}

/// The test-only constructor can supply loopback transport and a real worker
/// executable. Production reaches this engine only through the WSS factory.
pub(super) async fn serve(
    config: Config,
    rest: RestClient,
    mut shutdown: watch::Receiver<()>,
) -> Result<()> {
    for attempt in 0..5 {
        match run(&config, &rest, shutdown.clone()).await {
            Ok(()) => return Ok(()),
            Err(error) if error.is::<client::ContainmentError>() => return Err(error),
            Err(_) => {
                tracing::warn!("terminal transport stopped; retrying with durable audit state")
            }
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(1 << attempt)) => {},
        }
    }
    bail!("terminal transport retries exhausted")
}

async fn run(config: &Config, rest: &RestClient, mut shutdown: watch::Receiver<()>) -> Result<()> {
    let journal = Journal::open(config.journal.clone(), config.scope, config.owner)?;
    journal.recover(&config.keys)?;
    let mut connection = tokio::select! {
        _ = shutdown.changed() => return Ok(()),
        result = timeout(Duration::from_secs(45), NostrWsConnection::connect_authenticated(
            &config.relay, &config.keys, config.auth.as_ref())) => result??,
    };
    let snapshot = tokio::select! {
        _ = shutdown.changed() => return Ok(()),
        result = authority::snapshot(rest, config.owner, config.scope) => result?,
    };
    subscribe(&mut connection, config, &snapshot).await?;
    let start = Instant::now();
    let mut incoming = Rate::default();
    let mut outgoing = Rate::default();
    let mut active: Option<Session> = None;
    let mut refresh: Option<tokio::task::JoinHandle<RefreshResult>> = None;
    let mut refreshed = Instant::now();
    let mut next_open = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result: Result<()> = async {
        loop {
            if active.as_ref().is_some_and(|session| Instant::now() >= session.authority_deadline
                || Timestamp::now().as_secs() >= session.grant.expires_at() || session.idle.elapsed() >= IDLE) {
                finish(config, &journal, &mut active).await?;
            }
            if refresh.is_none() && refreshed.elapsed() >= REFRESH {
                refreshed = Instant::now();
                let rest = rest.clone();
                let owner = config.owner;
                let scope = config.scope;
                let grant = active.as_ref().map(|session| session.grant.clone());
                refresh = Some(tokio::spawn(async move {
                    let checked_at = Instant::now();
                    let generation = grant.as_ref().map(Grant::id);
                    let verdict = match grant {
                        Some(grant) => authority::refresh(&rest, owner, &grant).await,
                        None => authority::snapshot(&rest, owner, scope).await,
                    };
                    RefreshResult { generation, checked_at, verdict }
                }));
            }
            tokio::select! {
                _ = shutdown.changed() => break,
                ready = async { match refresh.as_mut() {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                } }, if refresh.is_some() => {
                    refresh = None;
                    let RefreshResult { generation, checked_at, verdict } = ready?;
                    if generation != active.as_ref().map(|session| session.grant.id()) { continue; }
                    match verdict {
                        Ok(snapshot) => {
                            if let Some(session) = &mut active {
                                // A late result cannot revive an expired lease.
                                if Instant::now() >= session.authority_deadline { finish(config, &journal, &mut active).await?; continue; }
                                session.authority_deadline = authority_deadline(&session.grant, checked_at);
                            }
                            if let Some(session) = &active {
                                tokio::time::timeout_at(session.authority_deadline, subscribe(&mut connection, config, &snapshot)).await??;
                            } else { subscribe(&mut connection, config, &snapshot).await?; }
                        }
                        Err(_) => { finish(config, &journal, &mut active).await?; }
                    }
                }
                message = connection.next_event(Duration::from_secs(30)) => {
                    match message? {
                        RelayMessage::Closed { subscription_id, .. } if subscription_id == SUBSCRIPTION => bail!("terminal subscription closed"),
                        RelayMessage::Event { subscription_id, event } if subscription_id == SUBSCRIPTION => {
                            let ms = elapsed(start);
                            if event.content.len() > 32 * 1024 || !incoming.allow(ms) { continue; }
                            if let Some(session) = &mut active {
                                if event.pubkey != session.grant.requester() { continue; }
                                let Ok(frame) = attach::decrypt(&config.keys, event.pubkey, config.scope, session.grant.id(), &event, Timestamp::now().as_secs()) else { continue; };
                                let sequence = frame.seq;
                                let acknowledged = frame.ack;
                                let packet = session.flow.receive(frame)?;
                                session.acknowledged = session.acknowledged.max(acknowledged);
                                if let Some(packet) = packet {
                                    match packet {
                                        Packet::Close => { finish(config, &journal, &mut active).await?; continue; }
                                        Packet::Open { .. } => bail!("terminal received a second Open"),
                                        Packet::Ack => {},
                                        packet if session.exited.is_none() => {
                                            let bytes = packet.bytes()?.len() as u64;
                                            if bytes > 0 || matches!(packet, Packet::Resize { .. }) { session.idle = Instant::now(); }
                                            session.record.bytes_in = session.record.bytes_in.saturating_add(bytes);
                                            tokio::time::timeout_at(session.authority_deadline, session.pty.send(packet)).await??;
                                            journal.save(&session.record)?;
                                        }
                                        _ => {},
                                    }
                                }
                                if sequence != 0 && outgoing.allow(ms) {
                                    send(&mut connection, config, session, &session.flow.acknowledgement()).await?;
                                }
                            } else if Instant::now() >= next_open {
                                next_open = Instant::now() + Duration::from_secs(1);
                                match open(&mut connection, config, rest, &journal, &event, &mut outgoing, start).await {
                                    Ok(session) => {
                                        // Admission latency must not push the first refresh past
                                        // the authority lease that began before its HTTP lookup.
                                        refreshed = session.authority_deadline.checked_sub(REFRESH + authority::AUTH_TIMEOUT).unwrap_or_else(Instant::now);
                                        active = Some(session);
                                    }
                                    Err(error) if error.is::<client::ContainmentError>() => return Err(error),
                                    Err(_) => tracing::info!("terminal request refused"),
                                }
                            }
                        }
                        _ => {},
                    }
                }
                worker = async { match active.as_mut() {
                    Some(session) => session.pty.next().await,
                    None => std::future::pending().await,
                } }, if active.as_ref().is_some_and(|session| session.exited.is_none() && session.output.is_none() && session.flow.writable()) => {
                    let session = active.as_mut().context("terminal session disappeared")?;
                    match worker? {
                        client::Event::Data(bytes) => {
                            session.idle = Instant::now();
                            session.output = Some(bytes);
                        }
                        client::Event::Ended { failed } => {
                            session.exited = Some(Instant::now());
                            if failed { tracing::info!("terminal worker ended with an operation failure"); }
                        }
                    }
                }
                _ = tick.tick() => {
                    let ms = elapsed(start);
                    if let Some(session) = &mut active {
                        if Timestamp::now().as_secs() >= session.grant.expires_at() || session.idle.elapsed() >= IDLE {
                            finish(config, &journal, &mut active).await?;
                            continue;
                        }
                        for frame in session.flow.retries(ms)? {
                            if outgoing.allow(ms) { send(&mut connection, config, session, &frame).await?; }
                        }
                        if session.output.is_some() && session.flow.writable() && outgoing.allow(ms) {
                            let bytes = session.output.take().context("terminal output disappeared")?;
                            session.record.bytes_out = session.record.bytes_out.saturating_add(bytes.len() as u64);
                            journal.save(&session.record)?;
                            queue(&mut connection, config, session, Packet::data(&bytes)?, ms).await?;
                        }
                        if session.exited.is_some_and(|at| at.elapsed() >= DRAIN)
                            || (session.close_sent && session.acknowledged >= session.last_sent) {
                            finish(config, &journal, &mut active).await?;
                            continue;
                        }
                        if session.exited.is_some() && !session.close_sent && session.acknowledged >= session.last_sent && outgoing.allow(ms) {
                            queue(&mut connection, config, session, Packet::Close, ms).await?;
                            session.close_sent = true;
                        } else if session.exited.is_none() && session.heartbeat.elapsed() >= REFRESH && session.flow.writable() && outgoing.allow(ms) {
                            queue(&mut connection, config, session, Packet::data(&[])?, ms).await?;
                            session.heartbeat = Instant::now();
                        }
                    } else {
                        // Transport failures leave the exact metadata event
                        // durable. The next idle tick retries after backoff.
                        if refreshed.elapsed() >= REFRESH {
                            if outbox(&mut connection, &journal).await.is_err() {
                                tracing::info!("terminal audit remains queued for retry");
                            }
                            journal.recover(&config.keys)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }.await;
    if let Some(task) = refresh {
        task.abort();
    }
    // Containment failures take precedence over network or authorization errors.
    let cleanup = finish(config, &journal, &mut active).await;
    if result
        .as_ref()
        .is_err_and(|error| error.is::<client::ContainmentError>())
    {
        return result;
    }
    cleanup?;
    // Failure remains durable and does not turn a completed close into success
    // without an audit retry record.
    if outbox(&mut connection, &journal).await.is_err() {
        tracing::info!("terminal audit retained for next startup");
    }
    let _ = timeout(IO_TIMEOUT, connection.disconnect()).await;
    result
}

#[cfg(test)]
mod tests;
