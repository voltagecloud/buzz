//! The agent activity capture route against the local test relay. Skipped
//! unless `BUZZ_TEST_RELAY` points at `.test-relay`.
//!
//! The relay takes activity frames only over WebSocket from an agent its
//! owner attested, so no frame is sent here; the archive's handling of
//! one is covered by `archive`'s unit tests.

use buzz_identity::Identity;
use buzz_relay_client::event::Verifier;
use buzz_relay_client::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn the_relay_accepts_the_observer_route() {
    let Ok(dir) = std::env::var("BUZZ_TEST_RELAY") else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    let alice = Arc::new(
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let mut live = Live::connect(
        &tokio::runtime::Handle::current(),
        origin,
        alice,
        Arc::new(Verifier::default()),
        LiveOptions::default(),
    );
    // Kinds other than activity and metrics are dropped.
    live.watch_observer(vec![24200, 44200, 9]);
    let observer = |live: &Live| {
        live.statuses()
            .into_iter()
            .find(|(key, _)| *key == RouteKey::Observer)
            .map(|(_, status)| status)
    };
    let status = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            live.next().await;
            match observer(&live) {
                Some(RouteStatus::Pending | RouteStatus::SettingUp) | None => {}
                Some(status) => return status,
            }
        }
    })
    .await
    .expect("the observer route settles");
    assert_eq!(status, RouteStatus::Live);

    // Turning capture off removes the route.
    live.watch_observer(Vec::new());
    assert_eq!(observer(&live), None);
}
