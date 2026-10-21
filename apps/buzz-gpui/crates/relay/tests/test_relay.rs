//! Runs against the local test relay (`scripts/test-relay.sh up && seed`).
//! Skipped unless `BUZZ_TEST_RELAY` points at `.test-relay` (the directory
//! holding `identities.json` and `seed.json`).

use buzz_identity::Identity;
use buzz_relay_client::*;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

struct Fixture {
    origin: RelayOrigin,
    identities: Value,
    seed: Value,
}

fn fixture() -> Option<Fixture> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{name}")).unwrap()).unwrap()
    };
    let identities = read("identities.json");
    Some(Fixture {
        origin: RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap(),
        seed: read("seed.json"),
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

    fn channel(&self, name: &str) -> String {
        self.seed["channels"][name].as_str().unwrap().to_string()
    }
}

fn filter(value: Value) -> Filter {
    value.as_object().unwrap().clone()
}

#[tokio::test]
async fn reads_a_channel_over_both_transports() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = fixture.identity("alice");
    let general = fixture.channel("general");

    let http = RelayHttp::new(fixture.origin.clone(), alice.clone());
    let info = http.information().await.unwrap();
    assert_eq!(info["self"].as_str().map(str::len), Some(64));

    // By id: other tests keep posting to #general, so the seeded message
    // is not always among the newest.
    let welcome = fixture.seed["events"]["welcome"].as_str().unwrap();
    let events = http
        .query(&[filter(
            json!({"kinds": [9], "#h": [general], "ids": [welcome], "limit": 1}),
        )])
        .await
        .unwrap();
    assert!(
        events.iter().any(|e| e.id.to_hex() == welcome),
        "{events:?}"
    );

    let socket = RelaySocket::connect(
        &tokio::runtime::Handle::current(),
        fixture.origin.clone(),
        alice,
        Arc::new(Verifier::default()),
        SocketOptions::default(),
    );
    let mut subscription = socket.subscribe(vec![filter(
        json!({"kinds": [9], "#h": [general], "ids": [welcome], "limit": 1}),
    )]);
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), subscription.next())
            .await
            .expect("no EOSE from the relay")
        {
            Some(SubscriptionEvent::Event(event)) => seen.push(event.id.to_hex()),
            Some(SubscriptionEvent::Eose) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(seen.iter().any(|id| id == welcome));
}

#[tokio::test]
async fn private_channels_are_invisible_to_non_members() {
    let Some(fixture) = fixture() else {
        return;
    };
    let core = fixture.channel("core-team");
    let query = |name: &str| {
        let http = RelayHttp::new(fixture.origin.clone(), fixture.identity(name));
        let core = core.clone();
        async move {
            http.query(&[filter(json!({"kinds": [9], "#h": [core], "limit": 10}))])
                .await
                .unwrap()
        }
    };
    assert!(!query("alice").await.is_empty());
    assert!(query("carol").await.is_empty());
}
