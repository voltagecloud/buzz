//! Thread views against the local test relay: the state machine's reads go
//! through the real reader, and strict pages must carry a 39007 whose
//! binding this client reproduces. Skipped unless `BUZZ_TEST_RELAY` points
//! at `.test-relay`.

use buzz_identity::Identity;
use buzz_relay_client::reader::ReaderOptions;
use buzz_relay_client::threads::{
    Direction, ReadStep, ThreadConfig, ThreadRead, ThreadState, ThreadStatus, thread_authority,
};
use buzz_relay_client::*;
use nostr::prelude::{EventId, PublicKey};
use serde_json::Value;
use std::sync::Arc;

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

    fn event(&self, name: &str) -> EventId {
        EventId::from_hex(self.seed["events"][name].as_str().unwrap()).unwrap()
    }
}

/// Runs reads the way the session will: through the reader, results back
/// into `complete`, admitted events back into `receive`.
async fn drive(
    reader: &Reader,
    state: &mut ThreadState,
    first: Option<ThreadRead>,
) -> Vec<ReadStep> {
    let mut steps = Vec::new();
    let mut next = first;
    while let Some(read) = next {
        steps.push(read.step);
        let result = reader.read(read.filters.clone(), read.options).await;
        let completion = state.complete(read.token, result);
        assert!(!completion.denied, "{:?} was denied", read.step);
        state.receive(&completion.admit);
        next = completion.next;
    }
    steps
}

#[tokio::test(flavor = "multi_thread")]
async fn loads_seeded_replies_through_a_verified_thread_window() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = fixture.identity("alice");
    let viewer = alice.public_key();
    let http = RelayHttp::new(fixture.origin.clone(), alice);
    let info = http.information().await.unwrap();
    let relay = PublicKey::from_hex(info["self"].as_str().unwrap()).unwrap();
    let reader = Reader::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(http),
        Arc::new(Verifier::default()),
        ReaderOptions::default(),
    );
    let general = fixture.seed["channels"]["general"]
        .as_str()
        .unwrap()
        .to_string();
    let welcome = fixture.event("welcome");
    let replies = [
        fixture.event("thread_reply_bob"),
        fixture.event("thread_reply_carol"),
    ];
    let config = |message_id, exact| ThreadConfig {
        channel: general.clone(),
        message_id,
        exact,
        relay_author: relay,
        signing_authority: Some(relay),
        viewer,
        authority_host: Some(thread_authority(fixture.origin.websocket()).unwrap()),
    };

    // Opened on the root with nothing cached.
    let mut state = ThreadState::new(config(welcome, false), Arc::default(), None);
    let first = state.refresh();
    let steps = drive(&reader, &mut state, first).await;
    assert_eq!(
        steps,
        [ReadStep::Selected, ReadStep::Root, ReadStep::Window]
    );
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Ready, "{:?}", snapshot.error);
    assert_eq!(snapshot.direction, Some(Direction::Older));
    assert_eq!(snapshot.root.as_ref().unwrap().id, welcome);
    let shown: Vec<EventId> = snapshot.replies.iter().map(|row| row.id).collect();
    for reply in replies {
        assert!(shown.contains(&reply), "{shown:?}");
    }
    assert!(!snapshot.can_load_more);
    // The seeded replies answer the root directly; other runs may have
    // added replies to them, which nest.
    let top: Vec<EventId> = snapshot.tree.top_level.iter().map(|row| row.id).collect();
    for reply in replies {
        assert!(top.contains(&reply), "{top:?}");
    }

    // Opened exactly on one reply, as a deep link would.
    let mut state = ThreadState::new(config(replies[0], true), Arc::default(), None);
    let first = state.refresh();
    let steps = drive(&reader, &mut state, first).await;
    assert_eq!(steps[..2], [ReadStep::Target, ReadStep::Overlays]);
    assert_eq!(steps.last(), Some(&ReadStep::Window));
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Ready, "{:?}", snapshot.error);
    assert_eq!(snapshot.target_status, Some(TargetStatus::Ready));
    assert_eq!(snapshot.target.as_ref().unwrap().id, replies[0]);
    assert_eq!(snapshot.root.as_ref().unwrap().id, welcome);
    let shown: Vec<EventId> = snapshot.replies.iter().map(|row| row.id).collect();
    for reply in replies {
        assert!(shown.contains(&reply), "{shown:?}");
    }
}
