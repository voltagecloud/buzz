//! Human sharing never recovers an ended thread; only the owner's or a live
//! delegated gateway's mention does (`mentions/tests/ended.rs`).
use super::*;
use buzz_backend_kubernetes::lifecycle::{
    CHECKPOINT, ENDED_AT, ENDED_REASON, GENERATION, LIFECYCLE, LIFECYCLE_ENDED,
};

/// An admitted shared task in a thread whose session ended gets the ordinary
/// `session_ended` refusal; the tombstone and its checkpoint stay for the
/// owner. Mutation: let the shared branch of `start_mention` request recovery
/// → the task restores the checkpoint and this test fails.
#[tokio::test]
async fn shared_tasks_never_recover_an_ended_thread() {
    let relay = Relay::start().await;
    let cluster = buzz_backend_kubernetes::sandbox_fake::Shared::default();
    let mut manager = super::super::manager();
    manager.client = buzz_backend_kubernetes::sandbox_fake::client(&cluster);
    manager.config.shared_tasks = true;
    manager.defaults = json!({"image": format!("example.invalid/agent@sha256:{}", "a".repeat(64)),
        "pod_options": {"active_deadline_seconds": 3600}})
    .as_object()
    .unwrap()
    .clone();
    crate::mobile_launch::Registration::new(
        owner().public_key(),
        agent().public_key(),
        json!({"relay_url": relay.url,
            "private_key_nsec": agent().secret_key().to_bech32().unwrap(),
            "auth_tag": tag(&owner(), &agent(), ""),
            "launch": {"command": "codex-acp", "args": [], "env": {},
                "owner_pubkey": owner().public_key()}}),
    )
    .unwrap()
    .save(manager.client.clone(), NAMESPACE)
    .await
    .unwrap();
    let channel = uuid::Uuid::new_v4();
    let created = Timestamp::now().as_secs() - 100;
    let mention = |signer: &Keys, root: Option<&Event>| {
        let mut tags = vec![
            Tag::parse(["h", &channel.to_string()]).unwrap(),
            Tag::public_key(agent().public_key()),
        ];
        if let Some(root) = root {
            tags.push(Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap());
        }
        event(signer, 9, tags, "@agent task", Timestamp::now().as_secs())
    };
    let root = mention(&owner(), None);
    let started = manager
        .start_mention(
            owner().public_key(),
            agent().public_key(),
            &relay.url,
            &root,
            None,
        )
        .await;
    assert_eq!(started["ok"], true, "{started}");
    let generation = {
        let mut state = cluster.lock().unwrap();
        let sandbox = state.sandboxes.values_mut().next().unwrap();
        let generation = sandbox["metadata"]["annotations"][GENERATION].clone();
        let key = format!(
            "dev/{}/6f9619ff-8b86-d011-b42d-00cf4fc964ff.tar.gz",
            generation.as_str().unwrap()
        );
        sandbox["spec"]["operatingMode"] = json!("Suspended");
        let annotations = &mut sandbox["metadata"]["annotations"];
        annotations[LIFECYCLE] = json!(LIFECYCLE_ENDED);
        annotations[ENDED_REASON] = json!("completed");
        // Ended well before the task, so only the branch keeps it ended.
        annotations[ENDED_AT] = json!(chrono::DateTime::from_timestamp(created as i64, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        annotations[CHECKPOINT] = json!(key);
        state.pods.clear();
        generation
    };

    // The relay state under which the stranger's task is admitted.
    let auth = buzz_sdk::nip_oa::parse_auth_tag(&tag(&owner(), &agent(), "")).unwrap();
    let mut events = vec![event(&agent(), 0, vec![auth], "{}", created)];
    for key in [&owner(), &stranger()] {
        events.push(event(key, 0, vec![], "{}", created));
    }
    events.push(
        buzz_sdk::sharing::Grant::builder(agent().public_key(), channel, true)
            .unwrap()
            .custom_created_at(Timestamp::from(created))
            .sign_with_keys(&owner())
            .unwrap(),
    );
    let mut roster = vec![Tag::parse(["d", &channel.to_string()]).unwrap()];
    let mut humans = vec![];
    for (key, role) in [
        (owner().public_key(), "member"),
        (stranger().public_key(), "member"),
        (agent().public_key(), "bot"),
    ] {
        roster.push(Tag::parse(["p", &key.to_hex(), "", role]).unwrap());
        if role == "member" {
            humans.push(Tag::parse(["member", &key.to_hex(), role]).unwrap());
        }
    }
    events.push(event(&manager_keys(), 39002, roster, "", created));
    events.push(event(&manager_keys(), 13534, humans, "", created));
    events.push(event(
        &manager_keys(),
        39000,
        vec![
            Tag::parse(["d", &channel.to_string()]).unwrap(),
            Tag::parse(["t", "stream"]).unwrap(),
        ],
        "",
        created,
    ));
    let task = mention(&stranger(), Some(&root));
    events.extend([root.clone(), task.clone()]);
    *relay.events.lock().unwrap() = events;

    let refused = tokio::time::timeout(
        Duration::from_secs(10),
        manager.start_mention(
            owner().public_key(),
            agent().public_key(),
            &relay.url,
            &task,
            None,
        ),
    )
    .await
    .unwrap();
    assert_eq!(refused["code"], "session_ended", "{refused}");
    let state = cluster.lock().unwrap();
    let sandbox = state.sandboxes.values().next().unwrap();
    assert_eq!(
        sandbox["metadata"]["annotations"][LIFECYCLE],
        LIFECYCLE_ENDED
    );
    assert_eq!(sandbox["metadata"]["annotations"][GENERATION], generation);
    assert!(!state.secrets.keys().any(|name| name.ends_with("-recovery")));
}
