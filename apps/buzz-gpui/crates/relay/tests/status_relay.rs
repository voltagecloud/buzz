//! Other people's statuses against the local test relay. Skipped unless
//! `BUZZ_TEST_RELAY` points at `.test-relay`. Bob sets and clears a
//! status; Alice's session follows it.

use buzz_identity::Identity;
use buzz_relay_client::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

fn fixture() -> Option<(RelayOrigin, Arc<Identity>, Arc<Identity>)> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    let identity = |name: &str| {
        Arc::new(
            Identity::from_nsec(identities["identities"][name]["nsec"].as_str().unwrap()).unwrap(),
        )
    };
    Some((origin, identity("alice"), identity("bob")))
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_another_members_status() {
    let Some((origin, alice, bob)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let runtime = tokio::runtime::Handle::current();
    let open = |identity: &Arc<Identity>| {
        Session::open(
            &runtime,
            origin.clone(),
            identity.clone(),
            Store::in_memory().unwrap(),
        )
    };
    let (watcher, author) = (open(&alice), open(&bob));
    let bob_key = bob.pubkey_hex();
    let text = format!("Status test {}", std::process::id());
    author
        .set_status(StatusInput {
            text: text.clone(),
            emoji: "🧪".into(),
            expires_at: None,
        })
        .await
        .unwrap();

    // Read on watch.
    let watch = watcher.watch_statuses(std::slice::from_ref(&bob_key));
    let mut snapshot = watcher.snapshot();
    let shown = tokio::time::timeout(
        Duration::from_secs(20),
        snapshot.wait_for(|s| s.statuses.get(&bob_key).is_some_and(|s| s.text == text)),
    )
    .await
    .expect("the status is read")
    .unwrap()
    .statuses[&bob_key]
        .clone();
    assert_eq!(shown.emoji, "🧪");
    assert_eq!(shown.expires_at, None);

    // A clear arrives live once the socket is up.
    tokio::time::timeout(Duration::from_secs(20), snapshot.wait_for(|s| s.live))
        .await
        .expect("the live socket connects")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    author.set_status(StatusInput::default()).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        snapshot.wait_for(|s| !s.statuses.contains_key(&bob_key)),
    )
    .await
    .expect("the clear arrives live")
    .unwrap();

    // Dropping the last watch forgets the person.
    author
        .set_status(StatusInput {
            text: text.clone(),
            emoji: String::new(),
            expires_at: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        snapshot.wait_for(|s| s.statuses.contains_key(&bob_key)),
    )
    .await
    .expect("the new status arrives live")
    .unwrap();
    drop(watch);
    tokio::time::timeout(
        Duration::from_secs(5),
        snapshot.wait_for(|s| !s.statuses.contains_key(&bob_key)),
    )
    .await
    .expect("an unwatched status is dropped")
    .unwrap();

    author.set_status(StatusInput::default()).await.unwrap();
    watcher.dispose().await;
    author.dispose().await;
}
