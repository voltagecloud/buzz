//! Profile and status against the local test relay. Skipped unless
//! `BUZZ_TEST_RELAY` points at `.test-relay`. Leaving uses a throwaway
//! identity, admitted with an invite.

use buzz_identity::Identity;
use buzz_relay_client::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

fn fixture() -> Option<(RelayOrigin, Arc<Identity>)> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    let carol =
        Identity::from_nsec(identities["identities"]["carol"]["nsec"].as_str().unwrap()).unwrap();
    Some((origin, Arc::new(carol)))
}

async fn wait_for(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn saves_status_and_profile() {
    let Some((origin, carol)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let store = Store::in_memory().unwrap();
    let viewer = carol.pubkey_hex();
    let session = Session::open(
        &tokio::runtime::Handle::current(),
        origin.clone(),
        carol.clone(),
        store,
    );

    // Status: set, read back, clear.
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    session
        .set_status(StatusInput {
            text: "Testing settings".into(),
            emoji: "🧪".into(),
            expires_at: Some(expires),
        })
        .await
        .unwrap();
    let status = session.own_status().await.unwrap().unwrap();
    assert_eq!(status.text, "Testing settings");
    assert_eq!(status.emoji, "🧪");
    assert_eq!(status.expires_at, Some(expires));
    session.set_status(StatusInput::default()).await.unwrap();
    assert_eq!(session.own_status().await.unwrap(), None);

    // Profile: republish the current fields, which the session shows.
    let http = RelayHttp::new(origin.clone(), carol.clone());
    let filter = buzz_relay_client::account::own_profile_filter(&viewer);
    let events = http.query(&[filter]).await.unwrap();
    let current = events
        .iter()
        .max_by_key(|event| event.created_at)
        .map(|event| serde_json::from_str::<Value>(&event.content).unwrap())
        .unwrap_or(Value::Null);
    let text = |key: &str| current[key].as_str().unwrap_or_default().to_string();
    let name = Some(text("display_name"))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Carol".into());
    session
        .publish_profile(ProfileEdit {
            name: name.clone(),
            picture: text("picture"),
            about: text("about"),
        })
        .await
        .unwrap();
    let snapshot = session.snapshot();
    assert!(
        wait_for(|| snapshot
            .borrow()
            .profiles
            .get(&viewer)
            .is_some_and(|profile| profile.name == name))
        .await
    );

    // The availability choice reaches the viewer's own presence.
    session.watch_presence(vec![viewer.clone()]);
    session.set_presence_mode(PresenceMode::Away);
    assert!(
        wait_for(|| snapshot.borrow().presence.get(&viewer) == Some(&PresenceState::Away)).await
    );

    session.dispose().await;
}

/// A throwaway identity joins with an invite, publishes its first profile
/// over HTTP, opens a session and leaves. Leaving again finds it absent.
#[tokio::test(flavor = "multi_thread")]
async fn joins_with_an_invite_then_leaves() {
    let Some((origin, _)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let dir = std::env::var("BUZZ_TEST_RELAY").unwrap();
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    let alice =
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap();
    let owner = RelayHttp::new(origin.clone(), Arc::new(alice));
    let minted = owner
        .post(
            "/api/invites",
            serde_json::json!({"ttl_secs": 600, "max_uses": 1}).to_string(),
        )
        .await
        .unwrap();
    let minted: Value = serde_json::from_slice(&minted).unwrap();
    let code = minted["code"].as_str().unwrap();

    let newcomer = Arc::new(Identity::ephemeral());
    let http = RelayHttp::new(origin.clone(), newcomer.clone());
    assert!(!community::probe(&http).await.unwrap());
    let claimed = community::claim(&http, code, None, false).await.unwrap();
    assert_eq!(claimed, community::Claimed::Joined);

    // The profile step: nothing there yet, then the published profile.
    let found = community::inspect_profile(&http).await.unwrap().unwrap();
    assert_eq!(found.profile, None);
    let profile = ProfileEdit {
        name: "Throwaway".into(),
        picture: String::new(),
        about: "Here to leave.".into(),
    };
    community::publish_profile(&http, &profile, &found)
        .await
        .unwrap();
    let saved = community::inspect_profile(&http).await.unwrap().unwrap();
    assert_eq!(saved.profile, Some(profile));

    let session = Session::open(
        &tokio::runtime::Handle::current(),
        origin.clone(),
        newcomer.clone(),
        Store::in_memory().unwrap(),
    );
    let mut snapshot = session.snapshot();
    tokio::time::timeout(
        Duration::from_secs(20),
        snapshot.wait_for(|s| s.roster == RosterStatus::Ready),
    )
    .await
    .expect("the roster loads")
    .unwrap();
    assert_eq!(session.leave().await, Ok(account::LeaveOutcome::Left));
    session.dispose().await;
    assert!(!community::probe(&http).await.unwrap(), "membership ended");

    // A second leave, e.g. after a lost reply, still lets the app drop it.
    let again = Session::open(
        &tokio::runtime::Handle::current(),
        origin,
        newcomer,
        Store::in_memory().unwrap(),
    );
    assert_eq!(
        again.leave().await,
        Ok(account::LeaveOutcome::AlreadyAbsent)
    );
    again.dispose().await;
}

/// Another member's status reaches a session watching them, and their
/// clear arrives live.
#[tokio::test(flavor = "multi_thread")]
async fn watched_statuses_follow_live_changes() {
    let Ok(dir) = std::env::var("BUZZ_TEST_RELAY") else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    let open = |name: &str| {
        let nsec = identities["identities"][name]["nsec"].as_str().unwrap();
        Session::open(
            &tokio::runtime::Handle::current(),
            origin.clone(),
            Arc::new(Identity::from_nsec(nsec).unwrap()),
            Store::in_memory().unwrap(),
        )
    };
    let (bob, alice) = (open("bob"), open("alice"));
    let bob_id = bob.viewer().to_string();
    let snapshot = alice.snapshot();
    assert!(wait_for(|| snapshot.borrow().live).await);
    let _watch = alice.watch_statuses(std::slice::from_ref(&bob_id));
    let text = format!("Status test {}", std::process::id());
    bob.set_status(StatusInput {
        text: text.clone(),
        emoji: String::new(),
        expires_at: None,
    })
    .await
    .unwrap();
    assert!(
        wait_for(|| snapshot
            .borrow()
            .statuses
            .get(&bob_id)
            .is_some_and(|status| status.text == text))
        .await,
        "the watched status arrives"
    );
    bob.set_status(StatusInput::default()).await.unwrap();
    assert!(
        wait_for(|| !snapshot.borrow().statuses.contains_key(&bob_id)).await,
        "the clear arrives live"
    );
    bob.dispose().await;
    alice.dispose().await;
}
