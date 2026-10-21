//! The device cache and offline resume against the local test relay.
//! Skipped unless `BUZZ_TEST_RELAY` points at `.test-relay`.
//!
//! A session against the relay saves its roster and channel heads. A later
//! session for the same community whose relay does not answer shows them,
//! marked cached, and refuses to write.

use buzz_identity::Identity;
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
        Duration::from_secs(20),
        receiver.wait_for(|value| predicate(value)),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    .unwrap()
    .clone()
}

/// Copies one community's saved data to another origin, as if the same
/// relay now lived where nothing answers.
fn move_cache(store: &Store, from: &Partition, to: &Partition) {
    store
        .with(|c| {
            for table in ["head_events", "head_meta", "startup_cache"] {
                let columns: Vec<String> = c
                    .prepare(&format!("SELECT * FROM {table} LIMIT 0"))?
                    .column_names()
                    .iter()
                    .map(|name| name.to_string())
                    .filter(|name| name != "origin")
                    .collect();
                let list = columns.join(", ");
                c.execute(
                    &format!(
                        "INSERT OR REPLACE INTO {table} (origin, {list})
                         SELECT ?1, {list} FROM {table} WHERE origin = ?2 AND viewer = ?3"
                    ),
                    rusqlite::params![to.origin, from.origin, from.viewer],
                )?;
            }
            Ok(())
        })
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_conversations_show_while_the_relay_is_unreachable() {
    let Some((origin, identities, seed)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = Arc::new(
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let general = seed["channels"]["general"].as_str().unwrap().to_string();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("buzz.sqlite3")).unwrap();
    let online = Partition::new(&origin, &alice.pubkey_hex());

    // Online: the roster and the opened channel's head are saved.
    let session = Session::open(
        &tokio::runtime::Handle::current(),
        origin.clone(),
        alice.clone(),
        store.clone(),
    );
    let mut snapshot = session.snapshot();
    until(&mut snapshot, "the roster", |s| {
        s.roster == RosterStatus::Ready && s.channels.iter().any(|c| c.id == general)
    })
    .await;
    assert!(!snapshot.borrow().cached);
    let view = session.open_channel(&general);
    session.select(Some(general.clone()));
    let mut timeline = view.snapshot.clone();
    let shown = until(&mut timeline, "the head", |t| {
        t.status == TimelineStatus::Ready && !t.rows.is_empty()
    })
    .await;
    assert!(!shown.restored);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let heads = store.load_heads(&online, store::now_ms()).unwrap();
            let saved = store.startup_discovery(&online).unwrap();
            if heads.iter().any(|head| head.channel == general) && saved.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the head and roster were saved");
    // The sidebar preview comes from the open window.
    let preview = snapshot
        .borrow()
        .channels
        .iter()
        .find(|c| c.id == general)
        .and_then(|c| c.preview.clone());
    assert!(preview.is_some());
    drop(view);
    session.dispose().await;

    // Offline: nothing listens on port 1.
    let unreachable = RelayOrigin::parse("ws://127.0.0.1:1").unwrap();
    move_cache(
        &store,
        &online,
        &Partition::new(&unreachable, &alice.pubkey_hex()),
    );
    let session = Session::open(
        &tokio::runtime::Handle::current(),
        unreachable,
        alice.clone(),
        store.clone(),
    );
    let mut snapshot = session.snapshot();
    let resumed = until(&mut snapshot, "the saved roster", |s| {
        s.cached && s.channels.iter().any(|c| c.id == general)
    })
    .await;
    assert!(resumed.channels.iter().all(|c| c.cached), "display only");
    let failed = until(&mut snapshot, "the connection to fail", |s| {
        matches!(s.status, SessionStatus::Failed(_))
    })
    .await;
    assert!(failed.cached, "the saved snapshot stays");
    assert!(failed.relay.is_none() && !failed.live);

    let view = session.open_channel(&general);
    let mut timeline = view.snapshot.clone();
    let saved = until(&mut timeline, "the saved head", |t| !t.rows.is_empty()).await;
    assert!(saved.restored && saved.stale);
    assert_eq!(saved.status, TimelineStatus::Ready);

    // Nothing is writable, and nothing reads.
    let sent = session
        .send(&general, "offline", Mentions::default(), Vec::new(), None)
        .await;
    assert!(sent.is_err());
    assert_eq!(
        session.channel_state(&general).await.err().as_deref(),
        Some("Reconnect to update saved conversations.")
    );

    // A failed connection waits; the network coming back tries again.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(matches!(snapshot.borrow().status, SessionStatus::Failed(_)));
    session.online();
    until(&mut snapshot, "a new attempt", |s| {
        s.status == SessionStatus::Connecting
    })
    .await;
    until(&mut snapshot, "it to fail again", |s| {
        matches!(s.status, SessionStatus::Failed(_))
    })
    .await;

    // Clearing the cache wipes the saved copy.
    session.clear_cache().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        let partition = Partition::new(
            &RelayOrigin::parse("ws://127.0.0.1:1").unwrap(),
            &alice.pubkey_hex(),
        );
        while !store
            .load_heads(&partition, store::now_ms())
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the saved heads were wiped");
}
