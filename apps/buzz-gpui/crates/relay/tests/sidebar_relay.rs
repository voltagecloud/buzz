//! Sidebar preferences against the local test relay: the startup read,
//! then sort, star, group and mute writes, each undone afterwards.
//! Skipped unless `BUZZ_TEST_RELAY` points at `.test-relay`.

use buzz_identity::Identity;
use buzz_relay_client::sidebar::{NewSection, SortMode};
use buzz_relay_client::sidebar_store::{Destination, PreferencesStatus};
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
    for _ in 0..150 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn saves_sidebar_preferences() {
    let Some((origin, carol)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let session = Session::open(
        &tokio::runtime::Handle::current(),
        origin,
        carol,
        Store::in_memory().unwrap(),
    );
    let snapshot = session.snapshot();
    let ready = wait_for(|| {
        let current = snapshot.borrow();
        current.sidebar.status == PreferencesStatus::Ready
            && current.roster == RosterStatus::Ready
            && !current.channels.is_empty()
    })
    .await;
    assert!(ready, "{:?}", snapshot.borrow().sidebar.status);
    let channel = snapshot
        .borrow()
        .channels
        .iter()
        .find(|c| c.channel_type != Some(ChannelType::Dm))
        .unwrap()
        .id
        .clone();
    let data = || snapshot.borrow().sidebar.data.clone().unwrap();
    let before = data();

    // Sort, shown at once and confirmed.
    session
        .sidebar_sort("channels", SortMode::Recent)
        .await
        .unwrap();
    assert_eq!(data().sort_mode("channels"), SortMode::Recent);
    session
        .sidebar_sort("channels", before.sort_mode("channels"))
        .await
        .unwrap();

    // Star, then move into a new group, then back out of groups.
    session
        .sidebar_move(&channel, Destination::Starred)
        .await
        .unwrap();
    assert!(data().starred.contains(&channel));
    let section = uuid::Uuid::new_v4().to_string();
    session
        .sidebar_move(
            &channel,
            Destination::Create(NewSection {
                id: section.clone(),
                name: "Sidebar test".into(),
            }),
        )
        .await
        .unwrap();
    let moved = data();
    assert!(!moved.starred.contains(&channel));
    assert_eq!(moved.assignments.get(&channel), Some(&section));
    assert!(moved.sections.iter().any(|s| s.name == "Sidebar test"));
    session.sidebar_remove_section(&section).await.unwrap();
    let removed = data();
    assert!(!removed.sections.iter().any(|s| s.id == section));
    assert!(!removed.assignments.contains_key(&channel));
    if before.starred.contains(&channel) {
        session
            .sidebar_move(&channel, Destination::Starred)
            .await
            .unwrap();
    }

    // Mute and unmute.
    let muted = before.muted.contains(&channel);
    session.sidebar_mute(&channel, !muted).await.unwrap();
    assert_eq!(data().muted.contains(&channel), !muted);
    session.sidebar_mute(&channel, muted).await.unwrap();
    assert_eq!(data().muted.contains(&channel), muted);
    assert!(snapshot.borrow().sidebar.moves.is_empty());
}
