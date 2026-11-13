use std::sync::Arc;
use std::time::Duration;

use buzz_backend_kubernetes::provider;
use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Event, EventBuilder, Kind, Tag, Timestamp};
use serde_json::json;
use tokio::sync::Semaphore;

use super::journal::Journal;
use super::registry::Identity;
use super::{Error, IO_TIMEOUT};
use crate::manager::Manager;
use crate::mobile_launch::{GatewayRequest, Start};

struct GatewayAuthors {
    keys: Vec<nostr::PublicKey>,
    relay: Option<nostr::PublicKey>,
}

const PAGE: usize = 1000;
const MAX_PAGES: usize = 16;
/// Deadline for one launch attempt: the ordinary deploy and, in an ended
/// thread, the checkpoint recovery that follows it. The journal lease bounds
/// it (asserted below), so it is shorter than a recovery can take
/// ([`provider::RECOVERY_BUDGET_SECS`]: a Pod drain, then the binding and
/// startup waits). A slow restore ends the attempt as `launch_timeout`, which
/// is retried. The provider journals a recovery before it deletes the
/// tombstone, so a later attempt converges: its ordinary deploy reuses the
/// replacement the recovery created, or gets `recovery_interrupted` and
/// finishes the recovery from that journal. If the attempts run out first, the
/// reply asks for another mention, which converges the same way.
const TURN_TIMEOUT: Duration = Duration::from_secs(180);
// The note above holds while one attempt cannot cover a whole recovery.
const _: () = assert!(TURN_TIMEOUT.as_secs() < provider::RECOVERY_BUDGET_SECS);
const RECYCLE_AFTER: Duration = Duration::from_secs(60);
const MAX_ATTEMPTS: u8 = 3;
const MAX_EVENT_BYTES: usize = 60_000;

enum LaunchPermission {
    /// Deliverable; carries fresh gateway authority for the direct signer or
    /// the owner cryptographically proven by relay workflow provenance.
    Allowed(Option<GatewayRequest>),
    UnsupportedConversation,
    NoLongerRemote,
    GatewayNotMember,
}

// A worker must renew its fenced journal before its longest external effect
// can outlive the lease. IO includes delivery revalidation and result storage.
const _: () =
    assert!(super::journal::LEASE_SECONDS > TURN_TIMEOUT.as_secs() + 3 * IO_TIMEOUT.as_secs());

pub(super) async fn run(
    manager: Arc<Manager>,
    identity: Identity,
    holder: &str,
    activated: u64,
    permits: Arc<Semaphore>,
) {
    let mut delay = Duration::from_secs(1);
    loop {
        let acquired = tokio::time::timeout(
            IO_TIMEOUT,
            Journal::acquire(
                manager.cluster(),
                &identity,
                holder,
                activated.max(identity.registered_at),
            ),
        )
        .await;
        let result = match acquired {
            Ok(Ok(mut journal)) => session(&manager, &identity, &mut journal, &permits).await,
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Deadline),
        };
        match result {
            Ok(()) => delay = Duration::from_secs(1),
            Err(error) => {
                tracing::warn!(%error, agent = %identity.keys.public_key(), "mention listener interrupted; durable inbox retained");
                // Saturation and corrupt registrations require operator repair.
                // Discovery retries them at its bounded interval, never in a hot loop.
                if matches!(error, Error::Capacity | Error::Registration) {
                    return;
                }
                delay = (delay * 2).min(Duration::from_secs(60));
            }
        }
        tokio::time::sleep(delay).await;
    }
}

async fn connect(identity: &Identity) -> Result<NostrWsConnection, Error> {
    tokio::time::timeout(
        IO_TIMEOUT,
        NostrWsConnection::connect_authenticated(
            &identity.relay,
            &identity.keys,
            Some(&identity.auth),
        ),
    )
    .await
    .map_err(|_| Error::Deadline)?
    .map_err(|_| Error::Relay)
}

async fn send(connection: &mut NostrWsConnection, value: serde_json::Value) -> Result<(), Error> {
    tokio::time::timeout(IO_TIMEOUT, connection.send_raw(&value))
        .await
        .map_err(|_| Error::Deadline)?
        .map_err(|_| Error::Relay)
}

/// Live delivery is installed before paged history. The cursor advances only
/// after every page reaches EOSE; a truncated second fails without skipping it.
pub(super) async fn session(
    manager: &Manager,
    identity: &Identity,
    journal: &mut Journal,
    permits: &Semaphore,
) -> Result<(), Error> {
    drain(manager, identity, journal, permits).await?;
    let mut connection = connect(identity).await?;
    let channels = super::channels::read(&mut connection, identity).await?;
    if channels.is_empty() {
        tokio::time::sleep(RECYCLE_AFTER).await;
        return Ok(());
    }
    // A delegation granted later is seen on the next bounded recycle; a
    // revocation is enforced again at launch, so this snapshot only widens
    // what is journaled, never what starts.
    let gateways: Vec<nostr::PublicKey> = super::gateways::read(&mut connection, identity)
        .await?
        .iter()
        .map(buzz_sdk::gateway::Live::gateway)
        .collect();
    let relay = if gateways.is_empty() {
        None
    } else {
        Some(
            crate::shared_tasks::relay_key(&identity.relay)
                .await
                .map_err(|_| Error::Relay)?,
        )
    };
    let gateways = GatewayAuthors {
        keys: gateways,
        relay,
    };
    let mut connected_at = Timestamp::now().as_secs();
    let started = tokio::time::Instant::now();
    let mut filter =
        json!({"kinds": [9, 40002], "#h":channels, "#p": [identity.keys.public_key()]});
    if !manager.sharing_enabled() {
        let mut authors = vec![identity.owner];
        authors.extend(gateways.keys.iter().copied());
        authors.extend(gateways.relay);
        filter["authors"] = json!(authors);
    }
    let mut live = filter.clone();
    live["limit"] = json!(0);
    send(&mut connection, json!(["REQ", "mentions-live", live])).await?;
    let mut history_started = false;
    let mut history_done = false;
    let mut count = 0;
    let mut oldest = connected_at;
    let mut until = connected_at;
    let mut pages = 0;
    let mut progress = tokio::time::Instant::now();
    loop {
        if history_done && started.elapsed() >= RECYCLE_AFTER {
            return Ok(());
        }
        if !history_done && progress.elapsed() > IO_TIMEOUT {
            return Err(Error::Deadline);
        }
        let message = match connection.next_event(Duration::from_secs(5)).await {
            Ok(message) => message,
            Err(WsClientError::Timeout) => {
                continue;
            }
            Err(_) => {
                tracing::warn!("mention socket failed");
                return Err(Error::Relay);
            }
        };
        match message {
            RelayMessage::Event {
                subscription_id,
                event,
            } if matches!(
                subscription_id.as_str(),
                "mentions-live" | "mentions-history"
            ) =>
            {
                if subscription_id == "mentions-history" {
                    count += 1;
                    if count > PAGE {
                        return Err(Error::Capacity);
                    }
                    oldest = oldest.min(event.created_at.as_secs());
                }
                let actionable = eligible(
                    identity,
                    journal,
                    &event,
                    manager.sharing_enabled(),
                    Some(&gateways),
                );
                tracing::debug!(event_id=%event.id, agent=%identity.keys.public_key(), actionable,"received mention");
                if actionable {
                    let persisted = tokio::time::timeout(IO_TIMEOUT, journal.enqueue(*event))
                        .await
                        .map_err(|_| Error::Deadline)??;
                    if persisted {
                        drain(manager, identity, journal, permits).await?;
                    }
                }
                if subscription_id == "mentions-history" {
                    progress = tokio::time::Instant::now();
                }
            }
            RelayMessage::Eose { subscription_id }
                if subscription_id == "mentions-live" && !history_started =>
            {
                history_started = true;
                // Fence after live installation so discovery cannot open a
                // delivery gap before the finite history query begins.
                connected_at = Timestamp::now().as_secs();
                oldest = connected_at;
                until = connected_at;
                history(&mut connection, filter.clone(), journal.state.since, until).await?;
                progress = tokio::time::Instant::now();
            }
            RelayMessage::Eose { subscription_id }
                if subscription_id == "mentions-history" && !history_done =>
            {
                pages += 1;
                if count == PAGE {
                    // Inclusive boundary preserves all events sharing a timestamp.
                    // If a whole second fills a page, NIP-01 cannot paginate it.
                    if oldest >= until || pages >= MAX_PAGES {
                        return Err(Error::Capacity);
                    }
                    until = oldest;
                    count = 0;
                    history(&mut connection, filter.clone(), journal.state.since, until).await?;
                } else {
                    send(&mut connection, json!(["CLOSE", "mentions-history"])).await?;
                    tokio::time::timeout(IO_TIMEOUT, journal.checkpoint(connected_at))
                        .await
                        .map_err(|_| Error::Deadline)??;
                    history_done = true;
                }
                progress = tokio::time::Instant::now();
            }
            // Buzz acknowledges our explicit CLOSE. The live subscription
            // must keep running after history has finished and checkpointed.
            RelayMessage::Closed {
                subscription_id, ..
            } if subscription_id == "mention-channels"
                || (subscription_id == "mentions-history" && history_done) => {}
            RelayMessage::Closed {
                subscription_id, ..
            } => {
                tracing::warn!(%subscription_id, "mention subscription closed");
                return Err(Error::Relay);
            }
            RelayMessage::Auth { .. } => return Err(Error::Relay),
            _ => {}
        }
    }
}

async fn history(
    connection: &mut NostrWsConnection,
    mut filter: serde_json::Value,
    since: u64,
    until: u64,
) -> Result<(), Error> {
    filter["since"] = json!(since);
    filter["until"] = json!(until);
    filter["limit"] = json!(PAGE);
    send(connection, json!(["REQ", "mentions-history", filter])).await
}

/// Whether `event` may be journaled for launch.
///
/// `gateways` is the listener's current snapshot of live delegated gateways.
/// `None` defers that judgement for an already journaled event: delivery
/// re-verifies the delegation before any launch.
fn eligible(
    identity: &Identity,
    journal: &Journal,
    event: &Event,
    shared: bool,
    gateways: Option<&GatewayAuthors>,
) -> bool {
    if event.created_at.as_secs() < journal.state.activated
        || serde_json::to_vec(event).map_or(true, |v| v.len() > MAX_EVENT_BYTES)
    {
        return false;
    }
    if event.pubkey == identity.owner {
        return conversation(identity, event).is_ok();
    }
    let delegated = gateways.is_none_or(|gateways| {
        gateways.keys.contains(&event.pubkey)
            || gateways
                .relay
                .and_then(|relay| {
                    buzz_sdk::workflow::gateway_owner(event, relay, identity.keys.public_key())
                })
                .is_some_and(|owner| gateways.keys.contains(&owner))
    });
    if delegated && gateway_thread(identity, event).is_ok() {
        return true;
    }
    shared
        && buzz_sdk::sharing::task::scope(
            event,
            identity.keys.public_key(),
            Timestamp::now().as_secs(),
        )
        .is_ok()
}

fn start_payload(identity: &Identity, event: &Event) -> serde_json::Value {
    json!({"agent_pubkey":identity.keys.public_key(),"relay_url":identity.relay,"trigger":event})
}

/// The thread a non-owner signer's mention binds, as an owner mention would.
fn gateway_thread(identity: &Identity, event: &Event) -> Result<(String, String), Error> {
    Start::thread_of(
        start_payload(identity, event),
        identity.owner,
        event.pubkey,
        Timestamp::now().as_secs(),
    )
    .map_err(|_| Error::Registration)
}

fn conversation(identity: &Identity, event: &Event) -> Result<(String, String), Error> {
    if event.pubkey == identity.owner {
        let start = Start::admit(
            start_payload(identity, event),
            identity.owner,
            Timestamp::now().as_secs(),
        )
        .map_err(|_| Error::Registration)?;
        let (channel, root) = start.conversation();
        Ok((channel.into(), root.into()))
    } else if let Ok(scope) = buzz_sdk::sharing::task::scope(
        event,
        identity.keys.public_key(),
        Timestamp::now().as_secs(),
    ) {
        Ok((scope.channel.to_string(), scope.root.to_hex()))
    } else {
        gateway_thread(identity, event)
    }
}

/// The journal survives all cancellation points, including a successful deploy
/// whose result could not be saved. The provider converges on the same thread.
async fn drain(
    manager: &Manager,
    identity: &Identity,
    journal: &mut Journal,
    permits: &Semaphore,
) -> Result<(), Error> {
    while journal.state.pending.is_some() {
        let pending = journal.state.pending.as_ref().ok_or(Error::Storage)?;
        if !eligible(
            identity,
            journal,
            &pending.event,
            manager.sharing_enabled(),
            None,
        ) {
            return Err(Error::Registration);
        }
        if pending.notice.is_some() {
            // Atomically retain the reply in its own outbox and finish this
            // launch. Lost channel access must not block unrelated threads.
            tokio::time::timeout(IO_TIMEOUT, journal.complete())
                .await
                .map_err(|_| Error::Deadline)??;
            continue;
        }
        let wait = pending
            .retry_at
            .saturating_sub(Timestamp::now().as_secs())
            .min(60);
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let _permit = tokio::time::timeout(Duration::from_secs(60), permits.acquire())
            .await
            .map_err(|_| Error::Deadline)?
            .map_err(|_| Error::Deadline)?;
        let pending = journal.state.pending.as_mut().ok_or(Error::Storage)?;
        // Persist the attempt before any external effect; a crash consumes it.
        if pending.attempts >= MAX_ATTEMPTS {
            pending.notice = Some(failure_notice(identity, &pending.event, "retry_exhausted")?);
            tokio::time::timeout(IO_TIMEOUT, journal.save())
                .await
                .map_err(|_| Error::Deadline)??;
            continue;
        }
        pending.attempts += 1;
        let event = pending.event.clone();
        tokio::time::timeout(IO_TIMEOUT, journal.save())
            .await
            .map_err(|_| Error::Deadline)??;
        let result = match channel_launch_allowed(identity, &event).await {
            // A non-owner trigger needs a fresh live delegation unless the
            // manager may still admit it as a shared task.
            Ok(LaunchPermission::GatewayNotMember) => {
                json!({"ok": false, "code": "gateway_not_member"})
            }
            Ok(LaunchPermission::Allowed(None))
                if event.pubkey != identity.owner && !manager.sharing_enabled() =>
            {
                json!({"ok": false, "code": "gateway_not_delegated"})
            }
            Ok(LaunchPermission::Allowed(delegation)) => tokio::time::timeout(
                TURN_TIMEOUT,
                manager.start_mention(
                    identity.owner,
                    identity.keys.public_key(),
                    &identity.relay,
                    &event,
                    delegation.as_ref(),
                ),
            )
            .await
            .unwrap_or_else(|_| json!({"ok": false, "code": "launch_timeout"})),
            Ok(LaunchPermission::UnsupportedConversation) => {
                json!({"ok": false, "code": "unsupported_conversation"})
            }
            // Deletion or retyping revokes automatic starts. Let the current
            // runtime handle the message without injecting a remote reply.
            Ok(LaunchPermission::NoLongerRemote) => json!({"ok": true}),
            Err(_) => json!({"ok": false, "code": "delivery_unconfirmed"}),
        };
        tracing::debug!(event_id=%event.id,ok=result["ok"].as_bool(),code=result["code"].as_str(),"mention launch result");
        if result["ok"] == true {
            tokio::time::timeout(IO_TIMEOUT, journal.complete())
                .await
                .map_err(|_| Error::Deadline)??;
        } else {
            let pending = journal.state.pending.as_mut().ok_or(Error::Storage)?;
            let code = result["code"].as_str().unwrap_or("launch_failed");
            // `recovery_mode_conflict` is retried like `session_active`: a
            // concurrent desktop recovery won, and the retry's ordinary
            // deploy reuses its replacement.
            if matches!(
                code,
                "session_ended"
                    | "session_ending"
                    | "recovery_interrupted"
                    | "no_checkpoint"
                    | "config_differs"
                    | "launch_not_registered"
                    | "unauthorized_owner"
                    | "invalid_start"
                    | "unsupported_conversation"
                    | "shared_task_denied"
                    | "gateway_not_delegated"
                    | "gateway_not_member"
            ) || pending.attempts >= MAX_ATTEMPTS
            {
                pending.notice = Some(failure_notice(identity, &event, code)?);
            } else {
                pending.retry_at = Timestamp::now().as_secs() + 5 * u64::from(pending.attempts);
            }
            tokio::time::timeout(IO_TIMEOUT, journal.save())
                .await
                .map_err(|_| Error::Deadline)??;
        }
    }
    flush_notices(identity, journal).await
}

/// Bounded delivery attempts retain the exact signed event, including after a
/// lost acknowledgement. Exhausted replies remain available to operators.
async fn flush_notices(identity: &Identity, journal: &mut Journal) -> Result<(), Error> {
    let ids: Vec<_> = journal
        .state
        .outbox
        .iter()
        .filter(|notice| notice.attempts < MAX_ATTEMPTS)
        .take(4)
        .map(|notice| notice.event.id)
        .collect();
    for id in ids {
        let notice = journal
            .state
            .outbox
            .iter_mut()
            .find(|notice| notice.event.id == id)
            .ok_or(Error::Storage)?;
        if notice.event.pubkey != identity.keys.public_key() || notice.event.verify().is_err() {
            return Err(Error::Storage);
        }
        notice.attempts += 1;
        let event = notice.event.clone();
        tokio::time::timeout(IO_TIMEOUT, journal.save())
            .await
            .map_err(|_| Error::Deadline)??;
        // No subscriptions: waiting for OK cannot buffer another inbox's traffic.
        let published = tokio::time::timeout(IO_TIMEOUT, async {
            let mut publisher = connect(identity).await?;
            let ok = publisher
                .send_event(event)
                .await
                .map_err(|_| Error::Relay)?;
            if ok.accepted {
                Ok(())
            } else {
                Err(Error::Relay)
            }
        })
        .await
        .unwrap_or(Err(Error::Deadline));
        match published {
            Ok(()) => {
                journal.state.outbox.retain(|notice| notice.event.id != id);
                tokio::time::timeout(IO_TIMEOUT, journal.save())
                    .await
                    .map_err(|_| Error::Deadline)??;
            }
            Err(error) => {
                tracing::warn!(%error, event_id = %id, "startup reply retained in durable outbox")
            }
        }
    }
    Ok(())
}

fn failure_notice(identity: &Identity, event: &Event, code: &str) -> Result<Event, Error> {
    let (channel, root) = conversation(identity, event)?;
    let text = match code {
        // An owner or live-gateway mention gets `session_ended` only when the
        // session ended after it; any other mention never recovers. A newer
        // mention can. `recovery_mode_conflict` is posted only once its retries
        // run out. Each keeps the prefix that `buzz acp-bridge` recognizes
        // (`SESSION_ENDED_NOTICE`).
        "session_ended" | "recovery_interrupted" => "This thread's agent session ended. My owner, or a gateway they delegated, can restore its last checkpoint by mentioning me here; otherwise mention me in a new thread.",
        "session_ending" => "This thread's agent session ended and is still saving its checkpoint. Once that finishes, my owner or a gateway they delegated can restore it by mentioning me here; otherwise mention me in a new thread.",
        "no_checkpoint" => "This thread's agent session ended without a saved checkpoint, so it cannot be restored. Mention me in a new thread.",
        "recovery_mode_conflict" => "This thread's agent session ended, and another recovery of it is in progress, so its last checkpoint could not be restored. Mention me here again once that recovery finishes, or mention me in a new thread.",
        "config_differs" => "This thread has different agent settings. Continue its existing session or mention me in a new thread.",
        "launch_not_registered" => "Start this agent once from desktop to register its launch settings, then mention me again.",
        "unsupported_conversation" => "Automatic remote startup requires a channel thread. Mention me in a channel, or manage this conversation from desktop.",
        "shared_task_denied" => "Shared task refused: current sharing permission, channel membership, or handoff budget does not permit this request. Ask the agent owner to check sharing.",
        "gateway_not_member" => "This gateway is not a current posting member of this channel, so I did not start. Ask the channel owner to restore membership.",
        "gateway_not_delegated" => "This gateway key is not currently delegated by my owner, so I did not start. Ask my owner to check `buzz gateway list`.",
        "shared_task_retry" => "Current sharing authorization is unavailable. Mention me again to retry; this request has not been allowed to run.",
        _ => "Agent startup could not be confirmed. Mention me again to retry.",
    };
    let tags = [
        vec!["h", &channel],
        vec!["e", &root, "", "root"],
        vec!["e", &event.id.to_hex(), "", "reply"],
    ]
    .into_iter()
    .map(Tag::parse)
    .collect::<Result<Vec<_>, _>>()
    .map_err(|_| Error::Registration)?;
    EventBuilder::new(Kind::Custom(9), text)
        .tags(tags)
        .sign_with_keys(&identity.keys)
        .map_err(|_| Error::Registration)
}

/// Recheck delivery under the agent's current relay permissions, including
/// restored intents. DMs use conversation-scoped ACP sessions and cannot run
/// in this provider's immutable thread binding.
async fn channel_launch_allowed(
    identity: &Identity,
    trigger: &Event,
) -> Result<LaunchPermission, Error> {
    tokio::time::timeout(IO_TIMEOUT, async {
        let (channel, _) = conversation(identity,trigger)?;
        let mut connection = connect(identity).await?;
        // Fresh, per launch: a revoked or expired delegation never starts.
        let delegation = if trigger.pubkey == identity.owner {
            None
        } else {
            let relay = crate::shared_tasks::relay_key(&identity.relay).await.map_err(|_| Error::Relay)?;
            let workflow_relay = (trigger.pubkey == relay).then_some(relay);
            let requester = if workflow_relay.is_some() {
                buzz_sdk::workflow::gateway_owner(trigger, relay, identity.keys.public_key())
            } else {
                Some(trigger.pubkey)
            };
            let delegation = super::gateways::read(&mut connection, identity)
                .await?
                .into_iter()
                .find(|live| Some(live.gateway()) == requester);
            if let Some(live) = &delegation {
                if !super::gateways::member(&mut connection, identity,
                    channel.parse().map_err(|_| Error::Registration)?, live.gateway(), relay).await? {
                    return Ok(LaunchPermission::GatewayNotMember);
                }
            }
            delegation.map(|delegation| GatewayRequest { delegation, workflow_relay })
        };
        send(&mut connection, json!(["REQ", "mention-trigger", {"kinds":[9,40002],"ids":[trigger.id],"#h":[channel],"limit":1}])).await?;
        send(&mut connection, json!(["REQ", "mention-channel", {"kinds":[39000],"#d":[channel],"limit":1,"consistency":"strong"}])).await?;
        // Query the current coordinate without a runtime filter: an old remote
        // record must not survive a newer local record or coordinate deletion.
        let agent = identity.keys.public_key().to_hex();
        send(&mut connection, json!(["REQ", "mention-listing", {"kinds":[30177],"authors":[identity.owner],"#d":[agent],"limit":1,"consistency":"strong"}])).await?;
        let (mut message_seen, mut metadata, mut trigger_done, mut channel_done, mut listing_done, mut remote) = (false, None, false, false, false, false);
        for _ in 0..16 {
            match connection.next_event(IO_TIMEOUT).await.map_err(|_| Error::Relay)? {
                RelayMessage::Event { subscription_id, event } if subscription_id == "mention-trigger" => {
                    message_seen |= event.id == trigger.id && event.verify().is_ok();
                }
                RelayMessage::Event { subscription_id, event } if subscription_id == "mention-channel" => {
                    if event.kind.as_u16() != 39000 || event.verify().is_err()
                        || !event.tags.iter().any(|tag| tag.as_slice() == ["d", channel.as_str()]) { return Err(Error::Relay); }
                    metadata = Some(!event.tags.iter().any(|tag| {
                        let tag = tag.as_slice();
                        tag.first().map(String::as_str) == Some("hidden") || tag == ["t", "dm"] || tag == ["archived", "true"]
                    }));
                }
                RelayMessage::Event { subscription_id, event } if subscription_id == "mention-listing" => {
                    if event.kind.as_u16() != 30177 || event.pubkey != identity.owner || event.verify().is_err() {
                        return Err(Error::Relay);
                    }
                    let coordinates: Vec<_> = event.tags.iter().filter(|tag| tag.as_slice().first().map(String::as_str) == Some("d")).collect();
                    let runtimes: Vec<_> = event.tags.iter().filter(|tag| tag.as_slice().first().map(String::as_str) == Some("runtime")).collect();
                    remote = coordinates.len() == 1 && coordinates[0].as_slice() == ["d", &agent]
                        && runtimes.len() == 1 && runtimes[0].as_slice() == ["runtime", "voltage.remote-workspace/sandbox"];
                }
                RelayMessage::Eose { subscription_id } => {
                    trigger_done |= subscription_id == "mention-trigger";
                    channel_done |= subscription_id == "mention-channel";
                    listing_done |= subscription_id == "mention-listing";
                }
                RelayMessage::Closed { .. } | RelayMessage::Auth { .. } => return Err(Error::Relay),
                _ => {},
            }
            if trigger_done && channel_done && listing_done {
                if !remote { return Ok(LaunchPermission::NoLongerRemote); }
                return match (message_seen, metadata) {
                    (true, Some(true)) => Ok(LaunchPermission::Allowed(delegation)),
                    (true, Some(false)) => Ok(LaunchPermission::UnsupportedConversation),
                    _ => Err(Error::Relay),
                };
            }
        }
        Err(Error::Capacity)
    }).await.map_err(|_| Error::Deadline)?
}
