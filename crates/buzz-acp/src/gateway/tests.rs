//! A loopback `/query` relay that serves kind 30181 heads and kind:0 profiles,
//! so gateway checks run through the production [`RestClient`] seam.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nostr::{Event, EventBuilder, Keys, Kind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

mod workflow;

/// Fake relay state shared with the serving task.
pub(crate) struct FakeRelay {
    pub(crate) heads: Arc<Mutex<Vec<Event>>>,
    pub(crate) profiles: Arc<Mutex<Vec<Event>>>,
    pub(crate) fail: Arc<AtomicBool>,
    pub(crate) queries: Arc<AtomicUsize>,
    pub(crate) rest: RestClient,
    pub(crate) members: Arc<Mutex<Vec<PublicKey>>>,
    /// Authority reads that did not ask for writer consistency.
    replica_reads: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeRelay {
    fn admit(&self, members: &[PublicKey]) {
        *self.members.lock().unwrap() = members.to_vec();
    }
}

fn channel() -> uuid::Uuid {
    "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10".parse().unwrap()
}

impl Drop for FakeRelay {
    fn drop(&mut self) {
        self.task.abort();
        // A replica read is answered 400, which callers treat as a denial, so
        // a test expecting denial would pass without this check.
        if !std::thread::panicking() {
            assert_eq!(
                self.replica_reads.load(Ordering::SeqCst),
                0,
                "authority reads must bypass replicas"
            );
        }
    }
}

/// Start a fake relay; `/query` answers 403 while `fail` is set.
pub(crate) async fn fake_relay() -> FakeRelay {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let heads = Arc::new(Mutex::new(Vec::<Event>::new()));
    let profiles = Arc::new(Mutex::new(Vec::<Event>::new()));
    let members = Arc::new(Mutex::new(Vec::<PublicKey>::new()));
    let fail = Arc::new(AtomicBool::new(false));
    let queries = Arc::new(AtomicUsize::new(0));
    let replica_reads = Arc::new(AtomicUsize::new(0));
    let task = {
        let (heads, profiles, fail, queries, members, replica_reads) = (
            heads.clone(),
            profiles.clone(),
            fail.clone(),
            queries.clone(),
            members.clone(),
            replica_reads.clone(),
        );
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                // Read headers plus the Content-Length body, bounded in time
                // and size so a stalled client cannot block later requests.
                while let Ok(Ok(read)) = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    socket.read(&mut buffer),
                )
                .await
                {
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.len() > 1024 * 1024 {
                        break;
                    }
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length = text[..split]
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if request.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                queries.fetch_add(1, Ordering::SeqCst);
                let text = String::from_utf8_lossy(&request).to_string();
                let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                let filter: Value = serde_json::from_str::<Vec<Value>>(body)
                    .ok()
                    .and_then(|filters| filters.into_iter().next())
                    .unwrap_or_default();
                // Gateway lookups send their delegation, profile, and roster
                // reads through one helper that adds writer consistency, so
                // checking the delegation and roster reads covers all three.
                // Kind 0 is not checked: the harness's existing sibling-agent
                // profile check reads it too, from replicas, by design.
                let replica_read = (filter["kinds"] == json!([30181])
                    || filter["kinds"] == json!([39002]))
                    && filter["consistency"] != json!("strong");
                if replica_read {
                    replica_reads.fetch_add(1, Ordering::SeqCst);
                }
                let (status, payload) = if replica_read {
                    ("400 Bad Request", "{}".to_string())
                } else if fail.load(Ordering::SeqCst) {
                    ("403 Forbidden", "{}".to_string())
                } else if text.starts_with("GET ") {
                    let relay = Keys::parse(&"11".repeat(32)).unwrap();
                    ("200 OK", json!({"self":relay.public_key()}).to_string())
                } else if filter["kinds"] == json!([39002]) {
                    let relay = Keys::parse(&"11".repeat(32)).unwrap();
                    let mut tags = vec![nostr::Tag::parse(["d", &channel().to_string()]).unwrap()];
                    tags.extend(
                        members.lock().unwrap().iter().map(|key| {
                            nostr::Tag::parse(["p", &key.to_hex(), "", "member"]).unwrap()
                        }),
                    );
                    let roster = EventBuilder::new(Kind::Custom(39002), "")
                        .tags(tags)
                        .sign_with_keys(&relay)
                        .unwrap();
                    ("200 OK", serde_json::to_string(&[roster]).unwrap())
                } else if filter["kinds"] == json!([30181]) {
                    (
                        "200 OK",
                        serde_json::to_string(&*heads.lock().unwrap()).unwrap(),
                    )
                } else if filter["kinds"] == json!([0]) {
                    (
                        "200 OK",
                        serde_json::to_string(&*profiles.lock().unwrap()).unwrap(),
                    )
                } else {
                    ("200 OK", "[]".to_string())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        })
    };
    FakeRelay {
        heads,
        profiles,
        members,
        fail,
        queries,
        replica_reads,
        rest: RestClient {
            http: reqwest::Client::new(),
            base_url,
            keys: Keys::generate(),
            auth_tag_json: None,
        },
        task,
    }
}

/// An owner-signed grant for `gateway`, created `age` seconds ago.
pub(crate) fn grant(owner: &Keys, gateway: PublicKey, age: u64) -> Event {
    let now = unix_now();
    buzz_sdk::gateway::Delegation::grant(gateway, now + 3_600, now - age)
        .unwrap()
        .sign_with_keys(owner)
        .unwrap()
}

/// An owner-signed revocation for `gateway`, created `age` seconds ago.
pub(crate) fn revoke(owner: &Keys, gateway: PublicKey, age: u64) -> Event {
    buzz_sdk::gateway::Delegation::revoke(gateway, unix_now() - age)
        .unwrap()
        .sign_with_keys(owner)
        .unwrap()
}

/// The cached verdict is reused within the TTL, a revocation lands after it,
/// failures are refused and throttled, and agent keys never pass.
/// Mutations: drop the TTL reuse → the query count assertion fails; drop the
/// failure cache → the throttled-failure query count fails; drop the
/// `is_live_at` filter → the expired-cache assertion fails; drop the owner or
/// agent exclusion → their assertions fail.
#[tokio::test]
async fn gateway_access_is_cached_bounded_and_fails_closed() {
    let relay = fake_relay().await;
    let (owner, agent, gateway, sibling) = (
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
    );
    relay.admit(&[owner.public_key(), agent.public_key(), gateway.public_key()]);
    let access = GatewayAccess::new(
        owner.public_key(),
        agent.public_key(),
        relay.rest.clone(),
        channel(),
    );
    let gw = gateway.public_key();

    assert!(!access.is_live(&gw).await.unwrap(), "no delegation yet");
    relay.heads.lock().unwrap().push(grant(&owner, gw, 10));
    assert!(
        !access.is_live(&gw).await.unwrap(),
        "the empty verdict is reused within the TTL"
    );
    assert_eq!(relay.queries.load(Ordering::SeqCst), 1);
    skip(VERDICT_TTL).await;
    assert!(
        access.is_live(&gw).await.unwrap(),
        "grant seen after the TTL"
    );
    assert!(!access.is_live(&owner.public_key()).await.unwrap());
    assert!(!access.is_live(&agent.public_key()).await.unwrap());

    // A newer revocation wins after the TTL.
    relay.heads.lock().unwrap().push(revoke(&owner, gw, 5));
    skip(VERDICT_TTL).await;
    assert!(!access.is_live(&gw).await.unwrap(), "revoked");

    // An agent key the owner names is refused by its kind:0 attestation.
    relay
        .heads
        .lock()
        .unwrap()
        .push(grant(&owner, sibling.public_key(), 1));
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &sibling.public_key(), "").unwrap();
    relay.profiles.lock().unwrap().push(
        EventBuilder::new(Kind::Metadata, "{}")
            .tag(buzz_sdk::nip_oa::parse_auth_tag(&auth).unwrap())
            .sign_with_keys(&sibling)
            .unwrap(),
    );
    skip(VERDICT_TTL).await;
    assert!(!access.is_live(&sibling.public_key()).await.unwrap());

    // Failure: refused, and not retried before FAILURE_RETRY.
    relay.fail.store(true, Ordering::SeqCst);
    skip(VERDICT_TTL).await;
    assert!(access.is_live(&gw).await.is_err());
    let after_failure = relay.queries.load(Ordering::SeqCst);
    assert!(access.is_live(&gw).await.is_err());
    assert_eq!(relay.queries.load(Ordering::SeqCst), after_failure);

    // Recovery after the failure backoff.
    relay.fail.store(false, Ordering::SeqCst);
    relay.heads.lock().unwrap().clear();
    relay.heads.lock().unwrap().push(grant(&owner, gw, 1));
    skip(FAILURE_RETRY).await;
    assert!(access.is_live(&gw).await.unwrap());

    // Even a still-fresh cache cannot extend the signed expiration.
    let now = unix_now();
    let expired = buzz_sdk::gateway::Delegation::grant(gw, now, now - 100)
        .unwrap()
        .sign_with_keys(&owner)
        .unwrap();
    *access.cached.lock().await = Cached::Fresh {
        at: tokio::time::Instant::now(),
        authority: Authority {
            gateways: buzz_sdk::gateway::live_heads(owner.public_key(), &[expired], now - 1),
            relay: None,
        },
    };
    assert!(
        !access.is_live(&gw).await.unwrap(),
        "a cached grant expired"
    );
}

/// Advance the tokio clock without letting paused time fire IO timeouts.
pub(crate) async fn skip(duration: Duration) {
    tokio::time::pause();
    tokio::time::advance(duration).await;
    tokio::time::resume();
}

fn channel_message(signer: &Keys, agent: PublicKey) -> Event {
    EventBuilder::new(Kind::Custom(9), "please run the tests")
        .tags([
            nostr::Tag::parse(["h", "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10"]).unwrap(),
            nostr::Tag::public_key(agent),
        ])
        .sign_with_keys(signer)
        .unwrap()
}

/// Thread ingress: strict owner mode admits the owner and a live gateway in
/// channels; never in DMs, never after revocation, never on lookup failure,
/// and never in modes that do not enable gateways.
/// Mutations: drop the gateway branch → "gateway in channel" fails; drop the
/// `!is_dm` guard → "gateway in DM" fails; drop the `RespondTo::Owner` guard
/// → the owner-only assertion fails.
#[tokio::test]
async fn author_gate_admits_a_live_gateway_only_in_owner_mode_channels() {
    let relay = fake_relay().await;
    let (owner, agent, gateway, stranger) = (
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
    );
    relay
        .heads
        .lock()
        .unwrap()
        .push(grant(&owner, gateway.public_key(), 10));
    relay.admit(&[owner.public_key(), agent.public_key(), gateway.public_key()]);
    let access = Arc::new(GatewayAccess::new(
        owner.public_key(),
        agent.public_key(),
        relay.rest.clone(),
        channel(),
    ));
    let mut gate =
        crate::InboundAuthorGate::connect(&relay.rest, &agent.public_key().to_hex(), "test").await;
    gate.enable_gateways(Some(access));
    let cache = crate::OwnerCache::new(Some(owner.public_key().to_hex()));
    let allowlist = std::collections::HashSet::new();
    let owner_mode = crate::RespondTo::Owner;
    let allowed = |signer: &Keys, mode: crate::RespondTo, is_dm: bool| {
        let event = channel_message(signer, agent.public_key());
        let (gate, cache, allowlist, rest) = (&gate, &cache, &allowlist, &relay.rest);
        async move {
            gate.evaluate_for_test(&event, &mode, allowlist, is_dm, cache, rest)
                .await
                .allowed
        }
    };

    assert!(allowed(&owner, owner_mode.clone(), false).await, "owner");
    assert!(
        allowed(&gateway, owner_mode.clone(), false).await,
        "gateway in channel"
    );
    assert!(
        !allowed(&gateway, owner_mode.clone(), true).await,
        "gateway in DM"
    );
    assert!(
        !allowed(&stranger, owner_mode.clone(), false).await,
        "stranger"
    );
    assert!(
        !allowed(&gateway, crate::RespondTo::OwnerOnly, false).await,
        "gateways are honoured only in strict owner mode"
    );

    relay
        .heads
        .lock()
        .unwrap()
        .push(revoke(&owner, gateway.public_key(), 5));
    skip(VERDICT_TTL).await;
    assert!(
        !allowed(&gateway, owner_mode.clone(), false).await,
        "revoked"
    );

    relay.fail.store(true, Ordering::SeqCst);
    skip(VERDICT_TTL).await;
    assert!(
        !allowed(&gateway, owner_mode.clone(), false).await,
        "lookup failure"
    );
    assert!(allowed(&owner, owner_mode, false).await, "owner unaffected");
}

fn control_frame(sender: &Keys, agent: &Keys, command: &str) -> Event {
    let content = crate::encrypt_observer_payload(
        sender,
        &agent.public_key(),
        &serde_json::json!({"type": command, "channelId": channel()}),
    )
    .unwrap();
    buzz_sdk::build_agent_observer_frame(
        &agent.public_key().to_hex(),
        &agent.public_key().to_hex(),
        buzz_core::observer::OBSERVER_FRAME_CONTROL,
        &content,
    )
    .unwrap()
    .sign_with_keys(sender)
    .unwrap()
}

/// Observer control: a live gateway may `cancel_turn` and nothing else; an
/// undelegated, revoked, or unverifiable sender is refused.
/// Mutations: drop the cancel-only restriction → "gateway switch_model"
/// fails; accept any non-owner sender → "stranger" fails; drop the gateway
/// lookup in `delegated_control_sender` → "gateway cancel" fails.
#[tokio::test]
async fn observer_control_from_a_gateway_is_cancel_only() {
    let relay = fake_relay().await;
    let (owner, agent, gateway, stranger) = (
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
    );
    relay
        .heads
        .lock()
        .unwrap()
        .push(grant(&owner, gateway.public_key(), 10));
    relay.admit(&[owner.public_key(), agent.public_key(), gateway.public_key()]);
    let access = GatewayAccess::new(
        owner.public_key(),
        agent.public_key(),
        relay.rest.clone(),
        channel(),
    );
    let owner_hex = owner.public_key().to_hex();
    let accepted = |sender: &Keys, command: &str| {
        let frame = control_frame(sender, &agent, command);
        let (access, owner_hex, agent) = (&access, &owner_hex, &agent);
        async move {
            let gateway = crate::delegated_control_sender(Some(access), &frame, owner_hex).await;
            crate::authorized_control_payload(agent, &frame, owner_hex, gateway).is_some()
        }
    };

    assert!(accepted(&owner, "cancel_turn").await, "owner cancel");
    assert!(accepted(&owner, "switch_model").await, "owner switch_model");
    assert!(accepted(&gateway, "cancel_turn").await, "gateway cancel");
    assert!(
        !accepted(&gateway, "switch_model").await,
        "gateway switch_model"
    );
    assert!(
        !accepted(&gateway, "publish_project_owner_announcements").await,
        "gateway announcements"
    );
    assert!(!accepted(&stranger, "cancel_turn").await, "stranger");

    relay
        .heads
        .lock()
        .unwrap()
        .push(revoke(&owner, gateway.public_key(), 5));
    skip(VERDICT_TTL).await;
    assert!(!accepted(&gateway, "cancel_turn").await, "revoked gateway");
}

/// Telemetry: while a delegation is live each frame is encrypted to the owner
/// and the gateway, and the extra frame is paid for by an idle tick so the
/// relay budget stays at one frame per tick.
/// Mutations: drop the gateway recipients → tick 1 has one frame; drop the
/// idle tick → tick 2 publishes.
#[tokio::test]
async fn telemetry_reaches_live_gateways_within_the_frame_budget() {
    let relay = fake_relay().await;
    let (owner, agent, gateway) = (Keys::generate(), Keys::generate(), Keys::generate());
    relay
        .heads
        .lock()
        .unwrap()
        .push(grant(&owner, gateway.public_key(), 10));
    relay.admit(&[owner.public_key(), agent.public_key(), gateway.public_key()]);
    let access = Arc::new(GatewayAccess::new(
        owner.public_key(),
        agent.public_key(),
        relay.rest.clone(),
        channel(),
    ));
    // Prime the cache in real time; the paced loop below runs on paused time.
    assert_eq!(access.live().await.unwrap(), vec![gateway.public_key()]);
    tokio::time::pause();

    let observer = crate::observer::ObserverHandle::in_process();
    for (channel, marker) in [(uuid::Uuid::new_v4(), "a"), (uuid::Uuid::new_v4(), "b")] {
        observer.emit(
            "test_event",
            None,
            &crate::observer::context_for(Some(channel), None, None),
            serde_json::json!({"marker": marker}),
        );
    }
    let (publisher, mut published) = crate::relay::RelayEventPublisher::test_pair();
    let rx = observer.subscribe();
    let task = tokio::spawn(crate::run_relay_observer_publisher(
        observer.snapshot(),
        rx,
        publisher,
        agent.clone(),
        agent.public_key().to_hex(),
        owner.public_key(),
        Some(access),
    ));
    // Let the publisher install its pacer before time moves.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    let first = tick(&mut published).await;
    assert_eq!(first.len(), 2, "owner and gateway copies");
    let recipients: Vec<PublicKey> = first
        .iter()
        .map(|frame| {
            let p = frame
                .tags
                .iter()
                .find(|tag| tag.as_slice().first().map(String::as_str) == Some("p"))
                .unwrap();
            PublicKey::from_hex(&p.as_slice()[1]).unwrap()
        })
        .collect();
    assert_eq!(recipients, vec![owner.public_key(), gateway.public_key()]);
    let decoded: Value = crate::decrypt_observer_payload(&gateway, &first[1]).unwrap();
    assert!(decoded.is_object(), "the gateway can read its copy");
    assert!(
        tick(&mut published).await.is_empty(),
        "idle tick pays for the gateway copy"
    );
    assert_eq!(
        tick(&mut published).await.len(),
        2,
        "next frame, again to both"
    );
    task.abort();
}

async fn tick(published: &mut tokio::sync::mpsc::Receiver<Event>) -> Vec<Event> {
    // The extra millisecond covers the timer wheel rounding a deadline that
    // was registered before the clock was paused.
    tokio::time::advance(crate::OBSERVER_PUBLISH_TICK + Duration::from_millis(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    let mut frames = Vec::new();
    while let Ok(frame) = published.try_recv() {
        frames.push(frame);
    }
    frames
}

/// Model dispatch rechecks a queued gateway turn: a revoked delegation or an
/// unverifiable lookup refuses it, the owner always passes, and a stranger is
/// refused without sharing.
/// Mutations: skip the gateway recheck → "revoked" passes; treat lookup
/// errors as live → "lookup failure" passes.
#[tokio::test]
async fn queued_gateway_turns_are_rechecked_at_dispatch() {
    let relay = fake_relay().await;
    let (owner, agent, gateway, stranger) = (
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
        Keys::generate(),
    );
    relay
        .heads
        .lock()
        .unwrap()
        .push(grant(&owner, gateway.public_key(), 10));
    let mut ctx = crate::pool::tests::make_prompt_context_no_owner();
    ctx.agent_owner_pubkey = Some(owner.public_key());
    relay.admit(&[owner.public_key(), agent.public_key(), gateway.public_key()]);
    ctx.gateways = Some(Arc::new(GatewayAccess::new(
        owner.public_key(),
        agent.public_key(),
        relay.rest.clone(),
        channel(),
    )));
    let channel = uuid::Uuid::new_v4();
    let batch = |signer: &Keys| crate::queue::FlushBatch {
        channel_id: channel,
        scope: crate::scope::SessionScope::Thread {
            channel_id: channel,
            root_event_id: "a".repeat(64),
        },
        events: vec![crate::queue::BatchEvent {
            event: channel_message(signer, agent.public_key()),
            prompt_tag: "@mention".into(),
            received_at: std::time::Instant::now(),
            edit: None,
        }],
        cancelled_events: vec![],
        cancel_reason: None,
    };
    let refused = |signer: &Keys| {
        let mut pending = batch(signer);
        let ctx = &ctx;
        async move {
            crate::pool::dispatch_refusal(ctx, &mut pending)
                .await
                .is_some()
        }
    };

    assert!(!refused(&owner).await, "owner");
    assert!(!refused(&gateway).await, "live gateway");
    assert!(refused(&stranger).await, "stranger without sharing");
    relay
        .heads
        .lock()
        .unwrap()
        .push(revoke(&owner, gateway.public_key(), 5));
    skip(VERDICT_TTL).await;
    assert!(refused(&gateway).await, "revoked");
    // A permanently refused author cannot poison unrelated owner work,
    // including instructions merged after a cancellation.
    for arrangement in ["regular", "cancelled-refused", "interrupted-owner"] {
        let mut mixed = batch(&owner);
        let owner_id = mixed.events[0].event.id;
        let denied = batch(&gateway).events.remove(0);
        let stranger_event = batch(&stranger).events.remove(0);
        match arrangement {
            "cancelled-refused" => mixed.cancelled_events.extend([denied, stranger_event]),
            "interrupted-owner" => {
                mixed.cancelled_events = std::mem::take(&mut mixed.events);
                mixed.events.extend([denied, stranger_event]);
                mixed.cancel_reason = Some(crate::queue::CancelReason::Steer);
            }
            _ => mixed.events.extend([denied, stranger_event]),
        }
        assert!(
            crate::pool::dispatch_refusal(&ctx, &mut mixed)
                .await
                .is_none(),
            "refused gateway blocked owner dispatch"
        );
        assert_eq!(
            mixed
                .events
                .iter()
                .map(|item| item.event.id)
                .collect::<Vec<_>>(),
            vec![owner_id]
        );
        assert!(mixed.cancelled_events.is_empty());
        assert!(mixed.cancel_reason.is_none());
    }
    let mut refused_only = batch(&gateway);
    assert!(crate::pool::dispatch_refusal(&ctx, &mut refused_only)
        .await
        .is_some());
    assert!(
        refused_only.events.is_empty(),
        "definitive refusal was retained for futile batch retries"
    );
    relay.heads.lock().unwrap().pop();
    relay.fail.store(true, Ordering::SeqCst);
    skip(VERDICT_TTL).await;
    assert!(refused(&gateway).await, "lookup failure");
    let mut waiting = batch(&gateway);
    assert!(crate::pool::dispatch_refusal(&ctx, &mut waiting)
        .await
        .is_some());
    assert_eq!(
        waiting.events.len(),
        1,
        "indeterminate authority silently discarded retryable work"
    );
}

/// The same factory wires ingress, dispatch, observer control and telemetry.
/// A DM or unbound harness must never create that shared gateway capability.
#[tokio::test]
async fn gateway_factory_admits_only_bound_owner_channel_threads() {
    let relay = fake_relay().await;
    let owner = Keys::generate().public_key().to_hex();
    let thread = crate::config::BoundSession::Thread {
        channel_id: uuid::Uuid::new_v4(),
        thread_root: "a".repeat(64),
    };
    let dm = crate::config::BoundSession::DirectMessage {
        dm_channel_id: uuid::Uuid::new_v4(),
    };
    for (binding, mode, owner_hex, expected) in [
        (
            Some(thread.clone()),
            crate::RespondTo::Owner,
            Some(owner.as_str()),
            true,
        ),
        (
            Some(dm),
            crate::RespondTo::Owner,
            Some(owner.as_str()),
            false,
        ),
        (None, crate::RespondTo::Owner, Some(owner.as_str()), false),
        (
            Some(thread.clone()),
            crate::RespondTo::OwnerOnly,
            Some(owner.as_str()),
            false,
        ),
        (Some(thread), crate::RespondTo::Owner, None, false),
    ] {
        let mut config = crate::build_mcp_servers_tests::test_config();
        config.bound_session = binding;
        config.respond_to = mode;
        assert_eq!(
            crate::gateway_access_for(&config, owner_hex, relay.rest.clone()).is_some(),
            expected
        );
    }
}

/// A global grant never grants a private channel. Joining enables telemetry,
/// prompts and cancellation; removing membership disables all three on refresh.
#[tokio::test]
async fn channel_membership_gates_gateway_telemetry_prompts_and_cancellation() {
    let relay = fake_relay().await;
    let (owner, agent, gateway) = (Keys::generate(), Keys::generate(), Keys::generate());
    relay
        .heads
        .lock()
        .unwrap()
        .push(grant(&owner, gateway.public_key(), 10));
    relay.admit(&[owner.public_key(), agent.public_key()]);
    let access = Arc::new(GatewayAccess::new(
        owner.public_key(),
        agent.public_key(),
        relay.rest.clone(),
        channel(),
    ));
    let mut gate =
        crate::InboundAuthorGate::connect(&relay.rest, &agent.public_key().to_hex(), "test").await;
    gate.enable_gateways(Some(access.clone()));
    let owner_hex = owner.public_key().to_hex();
    let owner_cache = crate::OwnerCache::new(Some(owner_hex.clone()));
    let allowlist = std::collections::HashSet::new();
    for allowed in [false, true, false] {
        if allowed {
            relay.admit(&[owner.public_key(), agent.public_key(), gateway.public_key()]);
        } else {
            relay.admit(&[owner.public_key(), agent.public_key()]);
        }
        skip(VERDICT_TTL).await;
        assert_eq!(
            access.is_live(&gateway.public_key()).await.unwrap(),
            allowed
        );
        let prompt = channel_message(&gateway, agent.public_key());
        assert_eq!(
            gate.evaluate_for_test(
                &prompt,
                &crate::RespondTo::Owner,
                &allowlist,
                false,
                &owner_cache,
                &relay.rest
            )
            .await
            .allowed,
            allowed
        );
        let control = control_frame(&gateway, &agent, "cancel_turn");
        let delegated = crate::delegated_control_sender(Some(&access), &control, &owner_hex).await;
        assert_eq!(
            crate::authorized_control_payload(&agent, &control, &owner_hex, delegated).is_some(),
            allowed
        );
        let recipients = membership_telemetry(access.clone(), &owner, &agent).await;
        let expected = if allowed {
            vec![owner.public_key(), gateway.public_key()]
        } else {
            vec![owner.public_key()]
        };
        assert_eq!(recipients, expected);
    }
}

async fn membership_telemetry(
    access: Arc<GatewayAccess>,
    owner: &Keys,
    agent: &Keys,
) -> Vec<PublicKey> {
    let observer = crate::observer::ObserverHandle::in_process();
    observer.emit(
        "test_event",
        None,
        &crate::observer::context_for(Some(channel()), None, None),
        serde_json::json!({"state":"private"}),
    );
    let (publisher, mut published) = crate::relay::RelayEventPublisher::test_pair();
    let task = tokio::spawn(crate::run_relay_observer_publisher(
        observer.snapshot(),
        observer.subscribe(),
        publisher,
        agent.clone(),
        agent.public_key().to_hex(),
        owner.public_key(),
        Some(access),
    ));
    let first = tokio::time::timeout(Duration::from_secs(2), published.recv())
        .await
        .unwrap()
        .unwrap();
    let mut frames = vec![first];
    if let Ok(Some(second)) =
        tokio::time::timeout(crate::OBSERVER_PUBLISH_TICK * 2, published.recv()).await
    {
        frames.push(second);
    }
    task.abort();
    frames
        .into_iter()
        .map(|frame| {
            frame
                .tags
                .iter()
                .find(|tag| tag.as_slice().first().map(String::as_str) == Some("p"))
                .and_then(|tag| PublicKey::from_hex(&tag.as_slice()[1]).ok())
                .unwrap()
        })
        .collect()
}
