//! The inbox against the local test relay. Skipped unless
//! `BUZZ_TEST_RELAY` points at `.test-relay`. Bob mentions alice in a
//! throwaway message in #random; alice's inbox shows it, reads it and
//! marks it unread again on this device only.

use buzz_identity::Identity;
use buzz_relay_client::unread::InboxStatus;
use buzz_relay_client::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

fn fixture() -> Option<(RelayOrigin, Value, Value)> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{name}")).unwrap()).unwrap()
    };
    let identities = read("identities.json");
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    Some((origin, identities, read("seed.json")))
}

async fn until<T: Clone>(
    receiver: &mut watch::Receiver<T>,
    what: &str,
    predicate: impl Fn(&T) -> bool,
) -> T {
    tokio::time::timeout(
        Duration::from_secs(30),
        receiver.wait_for(|value| predicate(value)),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    .unwrap()
    .clone()
}

fn session(origin: &RelayOrigin, identities: &Value, who: &str) -> Session {
    let identity = Arc::new(
        Identity::from_nsec(identities["identities"][who]["nsec"].as_str().unwrap()).unwrap(),
    );
    Session::open(
        &tokio::runtime::Handle::current(),
        origin.clone(),
        identity,
        Store::in_memory().unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn mentions_reach_the_inbox_and_read_there() {
    let Some((origin, identities, seed)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let random = seed["channels"]["random"].as_str().unwrap().to_owned();
    let alice_key = identities["identities"]["alice"]["pk_hex"]
        .as_str()
        .unwrap()
        .to_owned();
    let alice = session(&origin, &identities, "alice");
    let mut inbox = alice.inbox();
    until(&mut inbox, "the history read", |state| {
        state.feed.status == InboxFeedStatus::Ready && state.inbox.status == InboxStatus::Ready
    })
    .await;

    let bob = session(&origin, &identities, "bob");
    let mut bob_snapshot = bob.snapshot();
    until(&mut bob_snapshot, "bob's roster", |s| {
        s.channels.iter().any(|c| c.id == random)
    })
    .await;
    let marker = format!(
        "inbox test {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    bob.send(
        &random,
        &format!("nostr:{alice_key} {marker}"),
        Mentions {
            recipients: vec![alice_key.clone()],
            references: Vec::new(),
            ..Default::default()
        },
        Vec::new(),
        None,
    )
    .await
    .unwrap();

    let state = until(&mut inbox, "the mention in the inbox", |state| {
        state
            .inbox
            .items
            .iter()
            .any(|item| item.preview.contains(&marker) && item.mentioned)
    })
    .await;
    let item = state
        .inbox
        .items
        .iter()
        .find(|item| item.preview.contains(&marker))
        .unwrap()
        .clone();
    assert_eq!(item.channel_id, random);
    assert!(item.unread_count > 0 || item.manual);
    assert!(state.can_read);

    alice.read_inbox_item(item.clone()).await.unwrap();
    until(&mut inbox, "the row to read", |state| {
        state
            .inbox
            .items
            .iter()
            .any(|row| row.id == item.id && row.unread_count == 0 && !row.manual)
    })
    .await;
    let read = inbox
        .borrow()
        .inbox
        .items
        .iter()
        .find(|row| row.id == item.id)
        .unwrap()
        .clone();
    alice.mark_inbox_item_unread(read).await.unwrap();
    until(&mut inbox, "the row marked unread", |state| {
        state
            .inbox
            .items
            .iter()
            .any(|row| row.id == item.id && row.manual)
    })
    .await;

    // A fresh session's history read finds the mention too.
    let fresh = session(&origin, &identities, "alice");
    let mut again = fresh.inbox();
    until(&mut again, "the mention from history", |state| {
        state.feed.status == InboxFeedStatus::Ready
            && state
                .inbox
                .items
                .iter()
                .any(|row| row.preview.contains(&marker))
    })
    .await;
}
