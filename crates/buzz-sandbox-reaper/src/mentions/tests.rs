//! A black-box client sends only ordinary signed chat events. A real WebSocket
//! drives the production supervisor/worker and provider against a fake cluster.

mod gateway;
mod live_relay;
mod relay;
mod revoked;
mod shared_live;

use std::sync::Arc;
use std::time::Duration;

use buzz_backend_kubernetes::sandbox_fake;
use buzz_ws_client::NostrWsConnection;
use nostr::{nips::nip19::ToBech32, Event, EventBuilder, Keys, Kind, Tag, Timestamp};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{journal::Journal, registry::Identity, *};
use crate::manager::{Manager, ManagerConfig};
use crate::mobile_launch::Registration;

const CHANNEL: &str = "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10";
const NS: &str = "buzz-agent-staging-dev";
const BOUND: Duration = Duration::from_secs(20);

fn keys(byte: &str) -> Keys {
    Keys::parse(&byte.repeat(32)).unwrap()
}

fn manager(cluster: &sandbox_fake::Shared) -> Arc<Manager> {
    let config = ManagerConfig::parse(&json!({"version":1,"checkpoint_bucket":"fixture-checkpoints", "developers":{"dev":{
        "owner_pubkey":keys("33").public_key(), "namespace":NS,
        "profile_secret_arn":"arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdEf"
    }}}).to_string()).unwrap();
    Arc::new(
        Manager::new(
            keys("11"),
            config,
            sandbox_fake::client(cluster),
            crate::enroll::tests::offline(Default::default()),
        )
        .with_sandbox_defaults(
            json!({"image":format!("example.invalid/agent@sha256:{}", "a".repeat(64)),
            "pod_options":{"active_deadline_seconds":3600}})
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
}

async fn register(cluster: &sandbox_fake::Shared, relay: &str) -> Identity {
    let owner = keys("33");
    let agent = keys("22");
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
    Registration::new(owner.public_key(), agent.public_key(), json!({
        "relay_url":relay, "private_key_nsec":agent.secret_key().to_bech32().unwrap(), "auth_tag":auth,
        "launch":{"command":"codex-acp","args":[],"env":{},
            "policy_env":{"BUZZ_ACP_SESSION_POLICY":"thread"},"owner_pubkey":owner.public_key()}
    })).unwrap().save(sandbox_fake::client(cluster), NS).await.unwrap();
    registry::discover(&manager(cluster), relay)
        .await
        .unwrap()
        .pop_first()
        .unwrap()
        .1
}

fn message(content: &str, root: Option<&str>) -> Event {
    let mut tags = vec![
        Tag::parse(["h", CHANNEL]).unwrap(),
        Tag::public_key(keys("22").public_key()),
    ];
    if let Some(root) = root {
        tags.push(Tag::parse(["e", root, "", "reply"]).unwrap());
    }
    EventBuilder::new(Kind::Custom(9), content)
        .tags(tags)
        .sign_with_keys(&keys("33"))
        .unwrap()
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(BOUND, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn unchanged_client_starts_new_threads_and_reuses_replies_without_control_requests() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    register(&cluster, &relay.url).await;
    let stop = CancellationToken::new();
    let server = {
        let manager = manager(&cluster);
        let stop = stop.clone();
        let url = relay.url.clone();
        tokio::spawn(async move {
            crate::serve::run_services(manager, &url, None, true, stop.cancelled()).await
        })
    };
    relay.wait_subscription("mentions-history").await;
    let mut phone = NostrWsConnection::connect_authenticated(&relay.url, &keys("33"), None)
        .await
        .unwrap();
    let first = message("@agent first task", None);
    assert!(phone.send_event(first.clone()).await.unwrap().accepted);
    wait_for(|| cluster.lock().unwrap().sandboxes.len() == 1).await;
    let reply = message("@agent continue", Some(&first.id.to_hex()));
    assert!(phone.send_event(reply.clone()).await.unwrap().accepted);
    assert!(phone.send_event(first.clone()).await.unwrap().accepted);
    let second = message("@agent another task", None);
    assert!(phone.send_event(second.clone()).await.unwrap().accepted);
    wait_for(|| cluster.lock().unwrap().sandboxes.len() == 2).await;
    // Wait for the durable receipt, not merely the provider's first write.
    wait_for(|| completed(&cluster, &second)).await;
    assert!(completed(&cluster, &reply));
    assert!(relay
        .events
        .lock()
        .unwrap()
        .iter()
        .all(|e| e.kind.as_u16() != 24210));
    stop.cancel();
    tokio::time::timeout(BOUND, server).await.unwrap().unwrap();
    relay.stop().await;
}

fn inbox(cluster: &sandbox_fake::Shared) -> Value {
    use base64::Engine;
    cluster
        .lock()
        .unwrap()
        .secrets
        .values()
        .find_map(|secret| {
            secret["data"]["inbox.json"].as_str().map(|value| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .unwrap();
                serde_json::from_slice(&bytes).unwrap()
            })
        })
        .unwrap_or(Value::Null)
}

fn completed(cluster: &sandbox_fake::Shared, event: &Event) -> bool {
    inbox(cluster)["completed"].get(event.id.to_hex()).is_some()
}

#[tokio::test]
async fn requested_history_close_ack_keeps_the_live_listener_running() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let mut journal = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "close-ack",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    let manager = manager(&cluster);
    let mut listener = tokio::spawn(async move {
        worker::session(&manager, &identity, &mut journal, &Semaphore::new(1)).await
    });
    relay.wait_subscription("closed:mentions-history").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut listener)
            .await
            .is_err(),
        "an acknowledgement of our CLOSE must not terminate live delivery"
    );
    let mut phone = NostrWsConnection::connect_authenticated(&relay.url, &keys("33"), None)
        .await
        .unwrap();
    let event = message("@agent after history closed", None);
    assert!(phone.send_event(event.clone()).await.unwrap().accepted);
    wait_for(|| completed(&cluster, &event)).await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);
    assert_eq!(relay.count("mentions-live"), 1);
    listener.abort();
    let _ = listener.await;
    relay.stop().await;
}

#[tokio::test]
async fn durable_pending_launch_survives_restart_and_storage_failure_prevents_deploy() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let mut journal = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "same-process",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    let event = message("@agent persist first", None);
    relay.events.lock().unwrap().push(event.clone());
    cluster.lock().unwrap().fail_secret_updates = 1;
    assert_eq!(
        tokio::time::timeout(
            BOUND,
            worker::session(
                &manager(&cluster),
                &identity,
                &mut journal,
                &Semaphore::new(1)
            )
        )
        .await
        .unwrap(),
        Err(Error::Storage)
    );
    assert!(cluster.lock().unwrap().sandboxes.is_empty());
    let mut restored = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "same-process",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    assert!(restored.state.pending.is_none());
    restored.enqueue(event.clone()).await.unwrap();
    drop(restored);
    let mut restored = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "same-process",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    let manager = manager(&cluster);
    let task = tokio::spawn(async move {
        worker::session(&manager, &identity, &mut restored, &Semaphore::new(1)).await
    });
    wait_for(|| completed(&cluster, &event)).await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);
    task.abort();
    let _ = task.await;
    relay.stop().await;
}

#[tokio::test]
async fn leases_and_resource_versions_fence_competing_workers() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let mut old = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "one",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    assert!(matches!(
        Journal::acquire(
            sandbox_fake::client(&cluster),
            &identity,
            "two",
            Timestamp::now().as_secs()
        )
        .await,
        Err(Error::Lease)
    ));
    // Same-holder recreation is how the supervisor resumes an aborted worker.
    let mut next = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "one",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    next.enqueue(message("@agent next", None)).await.unwrap();
    assert_eq!(
        old.enqueue(message("@agent stale", None)).await,
        Err(Error::Lease)
    );
    relay.stop().await;
}

#[tokio::test]
async fn catchup_handles_disconnect_and_duplicate_history_without_starting_old_messages() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let journal = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "test",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    let old = EventBuilder::new(Kind::Custom(9), "@agent before activation")
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::public_key(keys("22").public_key()),
        ])
        .custom_created_at(Timestamp::from(journal.state.activated - 10))
        .sign_with_keys(&keys("33"))
        .unwrap();
    let event = message("@agent while disconnected", None);
    relay.events.lock().unwrap().extend([old, event.clone()]);
    for _ in 0..2 {
        let previous = relay.count("mentions-history");
        let mut journal = Journal::acquire(
            sandbox_fake::client(&cluster),
            &identity,
            "test",
            Timestamp::now().as_secs(),
        )
        .await
        .unwrap();
        let identity = register(&cluster, &relay.url).await;
        let manager = manager(&cluster);
        let task = tokio::spawn(async move {
            worker::session(&manager, &identity, &mut journal, &Semaphore::new(1)).await
        });
        relay.wait_after("mentions-history", previous).await;
        wait_for(|| completed(&cluster, &event)).await;
        task.abort();
        let _ = task.await;
    }
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);
    relay.stop().await;
}

#[tokio::test]
async fn tampered_mentions_and_display_references_never_launch() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let now = Timestamp::now().as_secs();
    let mut journal = Journal::acquire(sandbox_fake::client(&cluster), &identity, "test", now - 20)
        .await
        .unwrap();
    let mut tampered = message("@agent forged", None);
    tampered.content = "tampered after signing".into();
    let reference = EventBuilder::new(Kind::Custom(9), "about an agent")
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::parse(["mention", &keys("22").public_key().to_hex()]).unwrap(),
        ])
        .sign_with_keys(&keys("33"))
        .unwrap();
    let other_owner = EventBuilder::new(Kind::Custom(9), "@agent unauthorized")
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::public_key(keys("22").public_key()),
        ])
        .sign_with_keys(&keys("44"))
        .unwrap();
    let valid = EventBuilder::new(Kind::Custom(9), "@agent valid")
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::public_key(keys("22").public_key()),
        ])
        .custom_created_at(Timestamp::from(now - 5))
        .sign_with_keys(&keys("33"))
        .unwrap();
    relay
        .events
        .lock()
        .unwrap()
        .extend([tampered, reference, other_owner, valid.clone()]);
    let manager = manager(&cluster);
    let task = tokio::spawn(async move {
        worker::session(&manager, &identity, &mut journal, &Semaphore::new(1)).await
    });
    wait_for(|| completed(&cluster, &valid)).await;
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);
    task.abort();
    let _ = task.await;
    relay.stop().await;
}

#[tokio::test]
async fn registration_binds_owner_relay_key_and_attestation() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let secret: k8s_openapi::api::core::v1::Secret =
        serde_json::from_value(cluster.lock().unwrap().secrets[&identity.name].clone()).unwrap();
    assert!(Identity::read(secret.clone(), NS, keys("44").public_key(), &relay.url).is_err());
    assert!(Identity::read(
        secret.clone(),
        NS,
        keys("33").public_key(),
        "wss://another.community"
    )
    .is_err());
    for field in ["private_key_nsec", "auth_tag"] {
        let mut corrupt = secret.clone();
        let data = &mut corrupt
            .data
            .as_mut()
            .unwrap()
            .get_mut("launch.json")
            .unwrap()
            .0;
        let mut value: Value = serde_json::from_slice(data).unwrap();
        value["payload"][field] = json!("invalid");
        *data = serde_json::to_vec(&value).unwrap();
        assert!(Identity::read(corrupt, NS, keys("33").public_key(), &relay.url).is_err());
    }
    relay.stop().await;
}

#[tokio::test]
async fn ended_session_posts_an_ordinary_reply_without_recovering_the_sandbox() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let manager = manager(&cluster);
    let event = message("@agent original task", None);
    assert_eq!(
        manager
            .start_mention(
                identity.owner,
                identity.keys.public_key(),
                &identity.relay,
                &event,
                None,
            )
            .await["ok"],
        true
    );
    {
        let mut state = cluster.lock().unwrap();
        for sandbox in state.sandboxes.values_mut() {
            sandbox["metadata"]["annotations"][buzz_backend_kubernetes::lifecycle::LIFECYCLE] =
                json!("ended");
        }
        state.pods.clear();
    }
    let mut journal = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "test",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    let retry = message("@agent continue ended session", Some(&event.id.to_hex()));
    relay.events.lock().unwrap().push(retry.clone());
    journal.enqueue(retry.clone()).await.unwrap();
    relay
        .lose_next_ack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let first_identity = register(&cluster, &relay.url).await;
    let first_manager = Arc::clone(&manager);
    let previous = relay.count("mentions-history");
    let first = tokio::spawn(async move {
        worker::session(
            &first_manager,
            &first_identity,
            &mut journal,
            &Semaphore::new(1),
        )
        .await
    });
    relay.wait_after("mentions-history", previous).await;
    assert!(completed(&cluster, &retry));
    first.abort();
    let _ = first.await;
    let mut journal = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "test",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    assert!(journal.state.pending.is_none());
    assert_eq!(journal.state.outbox.len(), 1);
    let notice_id = journal.state.outbox[0].event.id;
    let task = tokio::spawn(async move {
        worker::session(&manager, &identity, &mut journal, &Semaphore::new(1)).await
    });
    wait_for(|| {
        inbox(&cluster)["outbox"]
            .as_array()
            .is_some_and(|v| v.is_empty())
    })
    .await;
    let notices: Vec<_> = relay
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.pubkey == keys("22").public_key())
        .cloned()
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].id, notice_id);
    assert!(notices[0].content.contains("Recover it from desktop"));
    assert_eq!(notices[0].kind.as_u16(), 9);
    assert!(notices[0]
        .tags
        .iter()
        .any(|tag| tag.as_slice() == ["e", &event.id.to_hex(), "", "root"]));
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);
    assert!(cluster.lock().unwrap().pods.is_empty());
    task.abort();
    let _ = task.await;
    relay.stop().await;
}

#[tokio::test]
async fn dm_metadata_produces_a_channel_instruction_without_starting_a_thread_sandbox() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let mut journal = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "test",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    let event = message("@agent direct message", None);
    let dm = EventBuilder::new(Kind::Custom(39000), "")
        .tags([
            Tag::parse(["d", CHANNEL]).unwrap(),
            Tag::parse(["t", "dm"]).unwrap(),
        ])
        .sign_with_keys(&keys("11"))
        .unwrap();
    {
        let mut events = relay.events.lock().unwrap();
        events.retain(|event| event.kind.as_u16() == 30177);
        events.extend([dm, event.clone()]);
    }
    journal.enqueue(event.clone()).await.unwrap();
    let manager = manager(&cluster);
    let task = tokio::spawn(async move {
        worker::session(&manager, &identity, &mut journal, &Semaphore::new(1)).await
    });
    wait_for(|| {
        completed(&cluster, &event)
            && inbox(&cluster)["outbox"]
                .as_array()
                .is_some_and(|v| v.is_empty())
    })
    .await;
    assert!(cluster.lock().unwrap().sandboxes.is_empty());
    assert!(relay
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event.pubkey == keys("22").public_key()
            && event.content.contains("requires a channel thread")));
    task.abort();
    let _ = task.await;
    relay.stop().await;
}

#[tokio::test]
async fn saturated_history_never_advances_its_cursor_past_unread_events() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let now = Timestamp::now().as_secs();
    let mut journal =
        Journal::acquire(sandbox_fake::client(&cluster), &identity, "test", now - 300)
            .await
            .unwrap();
    let before = journal.state.since;
    // Invalid channel bindings cannot launch, but still consume the relay's
    // history limit. Every event has a distinct signed ID at the same second.
    let events: Vec<_> = (0..1000)
        .map(|i| {
            EventBuilder::new(Kind::Custom(9), format!("{i}"))
                .tags([
                    Tag::parse(["h", CHANNEL]).unwrap(),
                    Tag::parse(["h", CHANNEL]).unwrap(),
                    Tag::public_key(keys("22").public_key()),
                ])
                .custom_created_at(Timestamp::from(now - 5))
                .sign_with_keys(&keys("33"))
                .unwrap()
        })
        .collect();
    relay.events.lock().unwrap().extend(events);
    assert_eq!(
        tokio::time::timeout(
            BOUND,
            worker::session(
                &manager(&cluster),
                &identity,
                &mut journal,
                &Semaphore::new(1)
            )
        )
        .await
        .unwrap(),
        Err(Error::Capacity)
    );
    let restored = Journal::acquire(sandbox_fake::client(&cluster), &identity, "test", now)
        .await
        .unwrap();
    assert_eq!(restored.state.since, before);
    assert!(cluster.lock().unwrap().sandboxes.is_empty());
    relay.stop().await;
}

#[tokio::test]
async fn undeliverable_notice_does_not_block_a_new_channel_launch() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let mut journal = Journal::acquire(
        sandbox_fake::client(&cluster),
        &identity,
        "test",
        Timestamp::now().as_secs(),
    )
    .await
    .unwrap();
    let notice = EventBuilder::new(Kind::Custom(9), "startup failed in a removed channel")
        .tags([Tag::parse(["h", "87dc5d01-5fb3-407e-936a-dc1d94ab192e"]).unwrap()])
        .sign_with_keys(&keys("22"))
        .unwrap();
    journal.state.outbox.push(journal::Notice {
        event: notice,
        attempts: 0,
    });
    journal.save().await.unwrap();
    let event = message("@agent new task despite failed notice", None);
    relay.events.lock().unwrap().push(event.clone());
    journal.enqueue(event.clone()).await.unwrap();
    let manager = manager(&cluster);
    let task = tokio::spawn(async move {
        worker::session(&manager, &identity, &mut journal, &Semaphore::new(1)).await
    });
    relay.wait_subscription("mentions-history").await;
    assert!(completed(&cluster, &event));
    assert_eq!(inbox(&cluster)["outbox"].as_array().unwrap().len(), 1);
    assert_eq!(inbox(&cluster)["outbox"][0]["attempts"], 1);
    assert_eq!(cluster.lock().unwrap().sandboxes.len(), 1);
    task.abort();
    let _ = task.await;
    relay.stop().await;
}
