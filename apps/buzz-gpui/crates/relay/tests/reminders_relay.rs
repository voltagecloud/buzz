//! Reminders (kind 30300) against the local test relay. Skipped unless
//! `BUZZ_TEST_RELAY` points at `.test-relay`. Each run writes one
//! throwaway reminder and closes it again; closed records expire.

use buzz_identity::Identity;
use buzz_relay_client::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

fn fixture() -> Option<(RelayOrigin, Value)> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    Some((origin, identities))
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test(flavor = "multi_thread")]
async fn reminders_save_sync_and_close() {
    let Some((origin, identities)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let carol = Arc::new(
        Identity::from_nsec(identities["identities"]["carol"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let open = || {
        Session::open(
            &tokio::runtime::Handle::current(),
            origin.clone(),
            carol.clone(),
            Store::in_memory().unwrap(),
        )
    };
    let (here, there) = (open(), open());
    let mut mine = here.reminders();
    let mut theirs = there.reminders();
    until(&mut mine, "the first list read", |s| {
        s.hydrated && s.status == RemindersStatus::Ready
    })
    .await;
    until(&mut theirs, "the other device's list", |s| {
        s.hydrated && s.status == RemindersStatus::Ready
    })
    .await;

    let marker = uuid_like();
    let target = ReminderTarget {
        event_id: marker.clone(),
        channel_id: "throwaway".into(),
        preview: "A throwaway reminder from the port's tests".into(),
        author_pubkey: carol.pubkey_hex(),
    };
    let due = now() + 3600;
    here.create_reminder(target, due, Some("  note  ".into()))
        .await
        .unwrap();
    let state = until(&mut mine, "the saved reminder", |s| {
        s.reminders
            .iter()
            .any(|r| r.target.as_ref().is_some_and(|t| t.event_id == marker))
    })
    .await;
    let saved = state
        .reminders
        .iter()
        .find(|r| r.target.as_ref().is_some_and(|t| t.event_id == marker))
        .unwrap()
        .clone();
    assert_eq!(saved.status, ReminderStatus::Pending);
    assert_eq!(saved.not_before, Some(due));
    assert_eq!(saved.note.as_deref(), Some("note"));

    // The other device hears it live.
    until(&mut theirs, "the live arrival", |s| {
        s.reminders.iter().any(|r| r.id == saved.id)
    })
    .await;

    here.snooze_reminder(&saved.id, due + 60).await.unwrap();
    here.cancel_reminder(&saved.id).await.unwrap();
    let closed = until(&mut theirs, "the cancellation", |s| {
        s.reminders
            .iter()
            .any(|r| r.id == saved.id && r.status == ReminderStatus::Cancelled)
    })
    .await;
    let head = closed.reminders.iter().find(|r| r.id == saved.id).unwrap();
    assert_eq!(head.not_before, None);
    assert_eq!(head.note.as_deref(), Some("note"));

    // A fresh read agrees with the live view.
    let fresh = open();
    let mut list = fresh.reminders();
    until(&mut list, "a fresh read with the closed head", |s| {
        s.hydrated
            && s.reminders
                .iter()
                .any(|r| r.id == saved.id && r.status == ReminderStatus::Cancelled)
    })
    .await;
}

fn uuid_like() -> String {
    let mut hex = String::new();
    while hex.len() < 64 {
        hex.push_str(&format!("{:x}", rand_u64()));
    }
    hex.truncate(64);
    hex
}

fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(now());
    hasher.finish()
}
