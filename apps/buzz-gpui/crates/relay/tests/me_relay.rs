//! Work sessions, the recipe catalog and Me placement against the local
//! test relay. Skipped unless `BUZZ_TEST_RELAY` points
//! at `.test-relay`. Every test signs in as throwaway members it invites
//! itself, so no seeded identity's records change.

use buzz_identity::Identity;
use buzz_relay_client::session::{AgentRules, Expected};
use buzz_relay_client::sidebar::NewSection;
use buzz_relay_client::sidebar_store::PreferencesStatus;
use buzz_relay_client::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

struct Fixture {
    origin: RelayOrigin,
    identities: Value,
}

fn fixture() -> Option<Fixture> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    Some(Fixture {
        origin: RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap(),
        identities,
    })
}

impl Fixture {
    fn identity(&self, name: &str) -> Arc<Identity> {
        let nsec = self.identities["identities"][name]["nsec"]
            .as_str()
            .unwrap();
        Arc::new(Identity::from_nsec(nsec).unwrap())
    }

    /// A throwaway community member: claims a fresh invite from alice.
    async fn newcomer(&self) -> Arc<Identity> {
        let owner = RelayHttp::new(self.origin.clone(), self.identity("alice"));
        let minted = owner
            .post(
                "/api/invites",
                json!({"ttl_secs": 600, "max_uses": 1}).to_string(),
            )
            .await
            .unwrap();
        let minted: Value = serde_json::from_slice(&minted).unwrap();
        let identity = Arc::new(Identity::ephemeral());
        let http = RelayHttp::new(self.origin.clone(), identity.clone());
        let code = minted["code"].as_str().unwrap();
        community::claim(&http, code, None, false).await.unwrap();
        identity
    }

    async fn session(&self, identity: Arc<Identity>) -> Session {
        let session = Session::open(
            &tokio::runtime::Handle::current(),
            self.origin.clone(),
            identity,
            Store::in_memory().unwrap(),
        );
        let mut snapshot = session.snapshot();
        until(&mut snapshot, "the roster", |s| {
            s.roster == RosterStatus::Ready && s.live
        })
        .await;
        session
    }
}

async fn until<T: Clone>(
    receiver: &mut watch::Receiver<T>,
    what: &str,
    predicate: impl Fn(&T) -> bool,
) -> T {
    tokio::time::timeout(
        Duration::from_secs(20),
        receiver.wait_for(|value| predicate(value)),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    .unwrap()
    .clone()
}

/// Creates a confirmed session and returns its id.
async fn start_session(session: &Session, title: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let op = session.work_session_create(&id, title, None).await.unwrap();
    session.work_session_delivered(op).await.unwrap();
    session
        .work_session_refresh(&id, Expected::default(), true)
        .await
        .unwrap();
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn places_conversations_in_me_and_groups_them() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let viewer = fixture.newcomer().await;
    let session = fixture.session(viewer).await;
    let kit = session.me_kit().clone();
    kit.refresh().await;
    assert!(kit.snapshot().ready(), "{:?}", kit.snapshot().status);
    assert!(kit.snapshot().me().is_none());

    let id = start_session(&session, "Plan the launch").await;
    let channel = session
        .channels_now()
        .iter()
        .find(|c| c.id == id)
        .cloned()
        .unwrap();
    assert_eq!(channel.channel_type, Some(ChannelType::Session));
    assert_eq!(channel.name, "Plan the launch");

    kit.admit(&id, None).await.unwrap();
    kit.set_placement(&id, true, None).await.unwrap();
    assert!(kit.placed(&id));
    assert_eq!(kit.snapshot().me_channels(), std::slice::from_ref(&id));

    // Groups: create one with a move, then delete it.
    kit.me_ensure();
    let mut me = kit.watch_me();
    until(&mut me, "Me groups", |s| {
        s.status == PreferencesStatus::Ready
    })
    .await;
    let group = uuid::Uuid::new_v4().to_string();
    // Replaceable records keep second precision; the relay needs a newer
    // second than the placement save.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    kit.me_create_and_assign(
        &id,
        NewSection {
            id: group.clone(),
            name: "Launch".into(),
        },
    )
    .await
    .unwrap();
    let state = kit.me_state();
    let data = state.data.as_ref().unwrap();
    assert_eq!(data.sections[0].name, "Launch");
    assert_eq!(data.assignments[&id], group);
    let saved = kit.snapshot().me().unwrap();
    assert_eq!(saved.assignments[&id], group);
    assert_eq!(saved.channels, std::slice::from_ref(&id));

    tokio::time::sleep(Duration::from_millis(1100)).await;
    kit.me_remove_section(&group).await.unwrap();
    let saved = kit.snapshot().me().unwrap();
    assert!(saved.groups.is_empty() && saved.assignments.is_empty());

    // Back to Messages.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    kit.set_placement(&id, false, None).await.unwrap();
    assert!(!kit.placed(&id));
    // A stale expected head is refused.
    let error = kit
        .save(
            json!({"type": "groups", "id": "me", "groups": [], "assignments": {}, "channels": []}),
            None,
            false,
        )
        .await
        .unwrap_err();
    assert!(error.starts_with("This saved recipe changed."), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn invites_and_adds_agents_to_a_session() {
    let Some(fixture) = fixture() else {
        return;
    };
    let viewer = fixture.newcomer().await;
    let first = fixture.newcomer().await.pubkey_hex();
    let second = fixture.newcomer().await.pubkey_hex();
    let session = fixture.session(viewer).await;
    let id = start_session(&session, "Agents").await;
    let rules = AgentRules {
        selectable: vec![first.clone(), second.clone()],
        archived: Vec::new(),
    };
    let refused = session
        .work_session_invite(&id, &first, &AgentRules::default())
        .await
        .unwrap_err();
    assert_eq!(refused, "Choose an agent from your agent library.");

    let op = session
        .work_session_invite(&id, &first, &rules)
        .await
        .unwrap();
    session.work_session_delivered(op).await.unwrap();
    session
        .work_session_refresh(&id, Expected::member(&first), true)
        .await
        .unwrap();
    assert!(session.lists_member(&id, &first).await.unwrap());
    assert!(!session.lists_member(&id, &second).await.unwrap());

    session
        .add_agents(&id, std::slice::from_ref(&second), &rules)
        .await
        .unwrap();
    assert!(session.lists_member(&id, &second).await.unwrap());
    assert!(
        session
            .member_channels(&second)
            .await
            .unwrap()
            .contains(&id)
    );
    let archived = AgentRules {
        archived: vec!["f".repeat(64)],
        selectable: vec!["f".repeat(64)],
    };
    assert_eq!(
        session
            .add_agents(&id, &["f".repeat(64)], &archived)
            .await
            .unwrap_err(),
        "This agent is archived. Choose another agent."
    );
}

/// Development only: claims a throwaway member and writes its nsec to
/// `BUZZ_NEWCOMER_FILE`, for screenshots that must not touch the seeded
/// identities. `cargo test --test me_relay -- --ignored mint_newcomer`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mint_newcomer() {
    let (Some(fixture), Ok(path)) = (fixture(), std::env::var("BUZZ_NEWCOMER_FILE")) else {
        return;
    };
    let identity = fixture.newcomer().await;
    std::fs::write(format!("{path}.pk"), identity.pubkey_hex()).unwrap();
    std::fs::write(path, identity.nsec().as_str()).unwrap();
}
