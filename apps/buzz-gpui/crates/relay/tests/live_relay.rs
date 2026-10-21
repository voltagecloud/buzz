//! Live routes against the local test relay. Skipped unless
//! `BUZZ_TEST_RELAY` points at `.test-relay`.

use buzz_identity::{Identity, Purpose};
use buzz_relay_client::event::Verifier;
use buzz_relay_client::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

fn fixture() -> Option<(RelayOrigin, Value, Value)> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{name}")).unwrap()).unwrap()
    };
    let identities = read("identities.json");
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    Some((origin, identities, read("seed.json")))
}

fn identity(identities: &Value, name: &str) -> Arc<Identity> {
    Arc::new(Identity::from_nsec(identities["identities"][name]["nsec"].as_str().unwrap()).unwrap())
}

async fn until<T>(live: &mut Live, mut pick: impl FnMut(LiveUpdate) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(found) = pick(live.next().await) {
                return found;
            }
        }
    })
    .await
    .expect("live update did not arrive")
}

#[tokio::test(flavor = "multi_thread")]
async fn channel_batches_deliver_new_messages() {
    let Some((origin, identities, seed)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let general = seed["channels"]["general"].as_str().unwrap().to_string();
    let random = seed["channels"]["random"].as_str().unwrap().to_string();
    let alice = identity(&identities, "alice");
    let mut live = Live::connect(
        &tokio::runtime::Handle::current(),
        origin.clone(),
        alice,
        Arc::new(Verifier::default()),
        LiveOptions::default(),
    );
    live.update(
        [general.clone(), random.clone()],
        [general.clone(), random.clone()],
        Vec::new(),
    )
    .unwrap();
    until(&mut live, |u| {
        matches!(u, LiveUpdate::Connected).then_some(())
    })
    .await;
    let established = until(&mut live, |u| match u {
        LiveUpdate::Established {
            route: RouteKey::Batch(_),
            channels,
            ..
        } => Some(channels),
        _ => None,
    })
    .await;
    assert!(established.contains(&general) && established.contains(&random));

    // Bob posts in #general over HTTP; alice's batch delivers it live.
    let bob = identity(&identities, "bob");
    let content = format!("live test {}", std::process::id());
    let unsigned = bob
        .template(
            Purpose::Content,
            9,
            nostr::prelude::Timestamp::now().as_secs(),
            vec![vec!["h".into(), general.clone()]],
            content.clone(),
        )
        .unwrap();
    let event = bob.sign(Purpose::Content, unsigned).unwrap();
    RelayHttp::new(origin, bob)
        .post("/events", serde_json::to_string(&event).unwrap())
        .await
        .unwrap();
    let received = until(&mut live, |u| match u {
        LiveUpdate::Events {
            events,
            phase: Phase::Live,
            ..
        } => events.into_iter().find(|e| e.content == content),
        _ => None,
    })
    .await;
    assert_eq!(received.channel(), Some(general.as_str()));
}
