//! A session against the TLS test relay, whose certificate only the test
//! CA vouches for. Skipped unless `BUZZ_TEST_RELAY_TLS` names the test
//! relay's state directory (`.test-relay`) after
//! `scripts/test-relay.sh up-tls`.
//!
//! The only test in this binary: it sets `SSL_CERT_FILE` for the process.

use buzz_identity::Identity;
use buzz_relay_client::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Live updates trust the same roots as HTTP reads. With the bundled web
/// roots only, the socket refused the test CA, every reconnect failed,
/// and a healthy relay showed "Live updates need attention".
#[test]
fn live_updates_connect_through_a_private_ca() {
    let Ok(dir) = std::env::var("BUZZ_TEST_RELAY_TLS") else {
        eprintln!("BUZZ_TEST_RELAY_TLS not set; skipping");
        return;
    };
    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{name}")).unwrap()).unwrap()
    };
    let tls = read("tls/relay.json");
    let identities = read("identities.json");
    // SAFETY: the only test in this binary, before any runtime thread.
    unsafe { std::env::set_var("SSL_CERT_FILE", tls["ca_bundle"].as_str().unwrap()) };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let alice = Arc::new(
            Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap())
                .unwrap(),
        );
        let origin = RelayOrigin::parse(tls["relay_ws_url"].as_str().unwrap()).unwrap();
        let general = tls["channels"]["general"].as_str().unwrap().to_string();
        let session = Session::open(
            &tokio::runtime::Handle::current(),
            origin,
            alice,
            Store::in_memory().unwrap(),
        );
        session.select(Some(general.clone()));
        let view = session.open_channel(&general);
        let mut snapshot = session.snapshot();
        let live = tokio::time::timeout(
            Duration::from_secs(20),
            snapshot.wait_for(|s| s.status == SessionStatus::Ready && s.live),
        )
        .await
        .is_ok();
        let current = snapshot.borrow().clone();
        assert!(
            live,
            "live updates never connected: {:?}",
            current.live_status
        );
        assert_eq!(current.live_status.issue(Some(&general)), None);

        // A message sent from another session arrives live.
        let other = Session::open(
            &tokio::runtime::Handle::current(),
            RelayOrigin::parse(tls["relay_ws_url"].as_str().unwrap()).unwrap(),
            Arc::new(
                Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap())
                    .unwrap(),
            ),
            Store::in_memory().unwrap(),
        );
        let mut ready = other.snapshot();
        tokio::time::timeout(
            Duration::from_secs(20),
            ready.wait_for(|s| s.status == SessionStatus::Ready && s.live),
        )
        .await
        .expect("the second session connects")
        .unwrap();
        let mut timeline = view.snapshot.clone();
        tokio::time::timeout(
            Duration::from_secs(20),
            timeline.wait_for(|t| t.status == TimelineStatus::Ready),
        )
        .await
        .expect("the channel loads")
        .unwrap();
        let text = format!("tls live {}", uuid_like());
        other
            .send(&general, &text, Mentions::default(), Vec::new(), None)
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(20),
            timeline.wait_for(|t| t.rows.iter().any(|row| row.content == text)),
        )
        .await
        .expect("the message arrives live")
        .unwrap();
    });
}

fn uuid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
