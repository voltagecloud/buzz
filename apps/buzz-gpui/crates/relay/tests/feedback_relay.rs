//! Product feedback against the local test relay. Skipped unless
//! `BUZZ_TEST_RELAY` points at `.test-relay`.

use buzz_identity::Identity;
use buzz_relay_client::feedback::Category;
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

#[tokio::test(flavor = "multi_thread")]
async fn sends_feedback_through_the_outbox() {
    let Some((origin, identities)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let carol = Arc::new(
        Identity::from_nsec(identities["identities"]["carol"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let session = Session::open(
        &tokio::runtime::Handle::current(),
        origin,
        carol,
        Store::in_memory().unwrap(),
    );
    let mut state = session.feedback();
    until(&mut state, "feedback to be available", |s| s.available).await;
    assert!(state.borrow().pending.is_none());

    let id = session
        .send_feedback(
            "Feedback from the port's tests",
            Some(Category::Praise),
            &[],
        )
        .await
        .unwrap();
    // A second one waits for the first to be cleared.
    assert!(session.send_feedback("again", None, &[]).await.is_err());

    // Never echoed: accepted is as far as it goes, and it stays pending.
    let accepted = until(&mut state, "the relay to accept feedback", |s| {
        s.pending
            .as_ref()
            .is_some_and(|p| p.delivery != Delivery::Sending)
    })
    .await;
    let pending = accepted.pending.unwrap();
    assert_eq!(pending.id, id);
    assert_eq!(pending.content, "Feedback from the port's tests");
    assert_eq!(pending.delivery, Delivery::Accepted, "{:?}", pending.error);

    session.dismiss_feedback(id).await.unwrap();
    until(&mut state, "the feedback to be cleared", |s| {
        s.pending.is_none()
    })
    .await;
}
