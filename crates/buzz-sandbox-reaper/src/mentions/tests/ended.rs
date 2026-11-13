//! Mentions in a thread whose Sandbox session ended. An owner or live
//! delegated-gateway mention is the explicit recovery request; it restores the
//! tombstone's checkpoint through the provider's recovery path, but only for a
//! session that ended before the mention.

use super::gateway::{delegation, roster};
use super::*;
use buzz_backend_kubernetes::lifecycle::{
    CHECKPOINT, CHECKPOINT_MISSING, ENDED_AT, ENDED_REASON, GENERATION, INITIAL_POD, LIFECYCLE,
    LIFECYCLE_ENDED, LIFECYCLE_ENDING, RECOVERED_FROM, RESTORE_CHECKPOINT,
};

const HOUR: u64 = 3600;

/// Start the owner's thread at `root`.
async fn start(manager: &Manager, identity: &Identity, root: &Event) {
    let started = deliver(manager, identity, root).await;
    assert_eq!(started["ok"], true, "{started}");
}

/// Tombstone the thread's one Sandbox as the reaper does at `ended_at` (Unix
/// seconds): Suspended, `ended`, Pod gone. Returns the ended generation and
/// the recorded checkpoint value.
fn end(cluster: &sandbox_fake::Shared, checkpoint: bool, ended_at: u64) -> (String, String) {
    let mut state = cluster.lock().unwrap();
    assert_eq!(state.sandboxes.len(), 1);
    let sandbox = state.sandboxes.values_mut().next().unwrap();
    let generation = sandbox["metadata"]["annotations"][GENERATION]
        .as_str()
        .unwrap()
        .to_owned();
    let key = if checkpoint {
        format!("dev/{generation}/6f9619ff-8b86-d011-b42d-00cf4fc964ff.tar.gz")
    } else {
        CHECKPOINT_MISSING.to_owned()
    };
    let ended_at = chrono::DateTime::from_timestamp(ended_at as i64, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    sandbox["spec"]["operatingMode"] = json!("Suspended");
    let annotations = &mut sandbox["metadata"]["annotations"];
    annotations[LIFECYCLE] = json!(LIFECYCLE_ENDED);
    annotations[ENDED_REASON] = json!("completed");
    annotations[ENDED_AT] = json!(ended_at);
    annotations[CHECKPOINT] = json!(key);
    state.pods.clear();
    (generation, key)
}

/// Start the owner's thread at `root`, then end it an hour ago.
async fn ended_thread(
    cluster: &sandbox_fake::Shared,
    manager: &Manager,
    identity: &Identity,
    root: &Event,
    checkpoint: bool,
) -> (String, String) {
    start(manager, identity, root).await;
    end(cluster, checkpoint, Timestamp::now().as_secs() - HOUR)
}

fn reply(signer: &Keys, root: &Event, content: &str, created_at: u64) -> Event {
    EventBuilder::new(Kind::Custom(9), content)
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::public_key(keys("22").public_key()),
            Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
        ])
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(signer)
        .unwrap()
}

/// A relay-signed scheduled prompt owned by the gateway, posted in `root`'s
/// thread (`mentions/tests/workflow.rs`).
fn scheduled_prompt(root: &Event) -> Event {
    EventBuilder::new(Kind::Custom(9), "@agent scheduled check-in")
        .tags([
            Tag::parse(["h", CHANNEL]).unwrap(),
            Tag::public_key(keys("22").public_key()),
            Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
            Tag::parse(["buzz:workflow", "true"]).unwrap(),
            Tag::parse([buzz_sdk::workflow::SCHEDULED_LITERAL_TAG, "literal-v1"]).unwrap(),
            Tag::parse(["buzz:workflow-owner", &keys("44").public_key().to_hex()]).unwrap(),
            Tag::parse(["buzz:workflow-mention", &keys("22").public_key().to_hex()]).unwrap(),
        ])
        .sign_with_keys(&keys("11"))
        .unwrap()
}

fn sandbox(cluster: &sandbox_fake::Shared) -> Value {
    let state = cluster.lock().unwrap();
    assert_eq!(state.sandboxes.len(), 1, "one Sandbox per thread");
    state.sandboxes.values().next().unwrap().clone()
}

fn recovery_journal(cluster: &sandbox_fake::Shared) -> bool {
    cluster
        .lock()
        .unwrap()
        .secrets
        .keys()
        .any(|name| name.ends_with("-recovery"))
}

/// Values of `key` in every launch Secret that sets it.
fn launch_env(cluster: &sandbox_fake::Shared, key: &str) -> Vec<String> {
    use base64::Engine;
    cluster
        .lock()
        .unwrap()
        .secrets
        .values()
        .filter_map(|secret| secret["data"][key].as_str())
        .map(|value| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(value)
                .unwrap();
            String::from_utf8(bytes).unwrap()
        })
        .collect()
}

/// The thread's Sandbox is still `before` and no recovery journal was written.
fn assert_not_recovered(cluster: &sandbox_fake::Shared, before: &Value) {
    assert_eq!(&sandbox(cluster), before);
    assert!(!recovery_journal(cluster));
}

/// The replacement restores `key`, and the mention is replayed into it: its
/// launch carries the mention's replay floor.
fn assert_recovered(cluster: &sandbox_fake::Shared, previous: &str, key: &str, trigger: &Event) {
    let recovered = sandbox(cluster);
    let annotations = &recovered["metadata"]["annotations"];
    assert!(annotations.get(LIFECYCLE).is_none(), "{annotations}");
    assert_eq!(annotations[RECOVERED_FROM], previous);
    assert_eq!(annotations[RESTORE_CHECKPOINT], key);
    assert_eq!(cluster.lock().unwrap().pods.len(), 1);
    assert!(
        !recovery_journal(cluster),
        "a bound replacement clears the recovery journal"
    );
    let restoring: Vec<_> = launch_env(cluster, "BUZZ_CHECKPOINT_RESTORE_KEY")
        .into_iter()
        .filter(|value| value == key)
        .collect();
    assert_eq!(restoring.len(), 1, "one launch restores the checkpoint");
    assert!(launch_env(cluster, "BUZZ_ACP_REPLAY_FLOOR")
        .contains(&trigger.created_at.as_secs().to_string()));
}

/// Replies the agent posted.
fn replies(relay: &relay::Relay) -> Vec<String> {
    relay
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.pubkey == keys("22").public_key())
        .map(|event| event.content.clone())
        .collect()
}

/// Drive the production worker for `trigger` until its result and any reply
/// are durable.
async fn run_worker(
    cluster: &sandbox_fake::Shared,
    relay: &relay::Relay,
    manager: Arc<Manager>,
    identity: Identity,
    trigger: &Event,
) {
    relay.events.lock().unwrap().push(trigger.clone());
    let mut journal = Journal::acquire(
        sandbox_fake::client(cluster),
        &identity,
        "test",
        Timestamp::now().as_secs() - 30,
    )
    .await
    .unwrap();
    journal.enqueue(trigger.clone()).await.unwrap();
    let task = tokio::spawn(async move {
        worker::session(&manager, &identity, &mut journal, &Semaphore::new(1)).await
    });
    wait_for(|| {
        completed(cluster, trigger)
            && inbox(cluster)["outbox"]
                .as_array()
                .is_some_and(|v| v.is_empty())
    })
    .await;
    task.abort();
    let _ = task.await;
}

#[derive(Clone, Copy, Debug)]
enum Signer {
    Owner,
    Gateway,
    RevokedGateway,
    GatewayWorkflow,
}

/// Each row drives the production worker (journal, permission recheck,
/// manager, provider) for one mention in a thread whose session ended an hour
/// ago, or is still ending. A still-ending session is left draining with the
/// `session_ending` reply. Mutations: start `recovers` false for the owner or
/// gateway branch in `start_mention` → their rows post `session_ended`
/// instead of recovering; recover for a scheduled workflow prompt → its row
/// recovers.
#[tokio::test]
async fn owner_and_live_gateway_mentions_recover_an_ended_thread() {
    for (signer, ending, notice) in [
        (Signer::Owner, false, None),
        (Signer::Gateway, false, None),
        (
            Signer::RevokedGateway,
            false,
            Some("not currently delegated"),
        ),
        (
            Signer::GatewayWorkflow,
            false,
            Some("This thread's agent session ended. My owner"),
        ),
        (Signer::Owner, true, Some("still saving its checkpoint")),
    ] {
        let relay = relay::Relay::start().await;
        let cluster = sandbox_fake::Shared::default();
        let identity = register(&cluster, &relay.url).await;
        let manager = manager(&cluster);
        let root = message("@agent original task", None);
        let (previous, key) = if ending {
            start(&manager, &identity, &root).await;
            let mut state = cluster.lock().unwrap();
            let sandbox = state.sandboxes.values_mut().next().unwrap();
            sandbox["metadata"]["annotations"][LIFECYCLE] = json!(LIFECYCLE_ENDING);
            (String::new(), String::new())
        } else {
            ended_thread(&cluster, &manager, &identity, &root, true).await
        };
        let now = Timestamp::now().as_secs();
        let trigger = match signer {
            Signer::Owner => reply(&keys("33"), &root, "@agent continue the thread", now),
            Signer::Gateway | Signer::RevokedGateway | Signer::GatewayWorkflow => {
                let mut events = relay.events.lock().unwrap();
                events.retain(|event| event.kind.as_u16() != 39002);
                events.push(roster(true));
                events.push(delegation(!matches!(signer, Signer::RevokedGateway), 10));
                match signer {
                    Signer::GatewayWorkflow => scheduled_prompt(&root),
                    _ => reply(&keys("44"), &root, "@agent continue the thread", now),
                }
            }
        };
        let before = sandbox(&cluster);
        run_worker(&cluster, &relay, manager, identity, &trigger).await;
        let replies = replies(&relay);
        match notice {
            None => {
                assert!(replies.is_empty(), "{signer:?}: {replies:?}");
                assert_recovered(&cluster, &previous, &key, &trigger);
            }
            Some(text) => {
                assert_eq!(replies.len(), 1, "{signer:?}: {replies:?}");
                assert!(replies[0].contains(text), "{signer:?}: {}", replies[0]);
                assert!(!replies[0].contains("desktop"), "{}", replies[0]);
                assert_not_recovered(&cluster, &before);
            }
        }
        relay.stop().await;
    }
}

async fn deliver(manager: &Manager, identity: &Identity, event: &Event) -> Value {
    manager
        .start_mention(
            identity.owner,
            identity.keys.public_key(),
            &identity.relay,
            event,
            None,
        )
        .await
}

/// Retries of one mention never undo an end that came after it. A recovery
/// interrupted after deleting the tombstone is finished by the retry from the
/// provider's recovery journal (`recovery_interrupted`); after a lost
/// acknowledgement the retry reuses the session it recovered; once the user
/// stops that session, the retry is refused, while a newer mention recovers.
/// A mention sent within the clock allowance of the end is refused too.
/// Mutations: treat only `session_ended` as recoverable → the interrupted
/// retry fails; send no `ended_before` from `Start::recovery` → the retry
/// after the stop recovers again; drop `MAX_CLOCK_LEAD_SECS` from it → the
/// last mention recovers.
#[tokio::test]
async fn a_mention_never_recovers_a_session_that_ended_after_it() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let manager = manager(&cluster);
    let now = Timestamp::now().as_secs();
    let root = message("@agent original task", None);
    start(&manager, &identity, &root).await;
    let (previous, key) = end(&cluster, true, now - HOUR);
    let trigger = reply(&keys("33"), &root, "@agent continue", now - HOUR / 2);
    cluster.lock().unwrap().fail_sandbox_creates = 1;
    let interrupted = deliver(&manager, &identity, &trigger).await;
    assert_eq!(interrupted["ok"], false, "{interrupted}");
    assert!(cluster.lock().unwrap().sandboxes.is_empty());
    assert!(recovery_journal(&cluster));
    assert_eq!(deliver(&manager, &identity, &trigger).await["ok"], true);
    assert_recovered(&cluster, &previous, &key, &trigger);
    let recovered = sandbox(&cluster);

    // The worker retries when the deploy's result was not journaled.
    assert_eq!(deliver(&manager, &identity, &trigger).await["ok"], true);
    assert_eq!(sandbox(&cluster), recovered, "the retry reuses the session");

    // The user stops the recovered session before another retry.
    let (generation, newer) = end(&cluster, true, now - HOUR / 4);
    let stopped = sandbox(&cluster);
    let again = deliver(&manager, &identity, &trigger).await;
    assert_eq!(again["code"], "session_ended", "{again}");
    assert_not_recovered(&cluster, &stopped);

    let next = reply(&keys("33"), &root, "@agent continue again", now);
    assert_eq!(deliver(&manager, &identity, &next).await["ok"], true);
    assert_recovered(&cluster, &generation, &newer, &next);

    // Its session ends; a mention 30 seconds later is within the allowance.
    end(&cluster, true, now);
    let ended = sandbox(&cluster);
    let early = reply(&keys("33"), &root, "@agent are you there", now + 30);
    let refused = deliver(&manager, &identity, &early).await;
    assert_eq!(refused["code"], "session_ended", "{refused}");
    assert_not_recovered(&cluster, &ended);
    relay.stop().await;
}

/// A desktop's fresh recovery wins the race with a mention's checkpoint
/// recovery. The mention's `recovery_mode_conflict` is retried, and the
/// retry's ordinary deploy reuses the desktop's replacement: no reply, no
/// second recovery. Mutation: make `recovery_mode_conflict` terminal in the
/// worker → a reply is posted and the replacement is never bound.
#[tokio::test]
async fn a_mention_that_loses_to_a_desktop_recovery_reuses_its_replacement() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let manager = manager(&cluster);
    let root = message("@agent original task", None);
    let (previous, _) = ended_thread(&cluster, &manager, &identity, &root, true).await;
    // The desktop's replacement appears as the mention deletes the tombstone.
    let generation = "f".repeat(32);
    let mut peer = sandbox(&cluster);
    peer["spec"]["operatingMode"] = json!("Running");
    let secret = &mut peer["spec"]["podTemplate"]["spec"]["containers"][0]["envFrom"][0]
        ["secretRef"]["name"];
    *secret = json!(secret.as_str().unwrap().replace(&previous, &generation));
    let annotations = peer["metadata"]["annotations"].as_object_mut().unwrap();
    for key in [LIFECYCLE, ENDED_REASON, ENDED_AT, CHECKPOINT, INITIAL_POD] {
        annotations.remove(key);
    }
    annotations.insert(GENERATION.into(), json!(generation));
    annotations.insert(RECOVERED_FROM.into(), json!(previous));
    cluster.lock().unwrap().peer_sandbox = Some(peer);

    let trigger = reply(
        &keys("33"),
        &root,
        "@agent continue",
        Timestamp::now().as_secs(),
    );
    run_worker(&cluster, &relay, manager, identity, &trigger).await;
    assert!(replies(&relay).is_empty(), "{:?}", replies(&relay));
    let replacement = sandbox(&cluster);
    let annotations = &replacement["metadata"]["annotations"];
    assert_eq!(annotations[GENERATION], generation.as_str());
    assert!(
        annotations.get(RESTORE_CHECKPOINT).is_none(),
        "{annotations}"
    );
    assert!(annotations.get(INITIAL_POD).is_some(), "the retry binds it");
    assert!(!recovery_journal(&cluster));
    relay.stop().await;
}

/// An owner mention cannot restore a tombstone without a checkpoint. Its reply
/// says so, is retained across a lost relay acknowledgement, and is published
/// exactly once; the tombstone stays in place.
#[tokio::test]
async fn ended_thread_without_a_checkpoint_posts_one_durable_reply() {
    let relay = relay::Relay::start().await;
    let cluster = sandbox_fake::Shared::default();
    let identity = register(&cluster, &relay.url).await;
    let manager = manager(&cluster);
    let event = message("@agent original task", None);
    ended_thread(&cluster, &manager, &identity, &event, false).await;
    let before = sandbox(&cluster);
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
    // `buzz acp-bridge` recognizes ended-session replies by this prefix.
    assert!(
        notices[0]
            .content
            .starts_with("This thread's agent session ended without a saved checkpoint"),
        "{}",
        notices[0].content
    );
    assert_eq!(notices[0].kind.as_u16(), 9);
    assert!(notices[0]
        .tags
        .iter()
        .any(|tag| tag.as_slice() == ["e", &event.id.to_hex(), "", "root"]));
    assert_not_recovered(&cluster, &before);
    task.abort();
    let _ = task.await;
    relay.stop().await;
}
