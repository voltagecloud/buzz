//! Presence, typing and notification candidates between two sessions on
//! the local test relay. Skipped unless `BUZZ_TEST_RELAY` points at
//! `.test-relay`. Uses #core-team, which no unread test reads.

use buzz_identity::{Identity, Purpose};
use buzz_relay_client::*;
use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
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

fn nsec<'a>(identities: &'a Value, name: &str) -> &'a str {
    identities["identities"][name]["nsec"].as_str().unwrap()
}

fn identity(identities: &Value, name: &str) -> Arc<Identity> {
    Arc::new(Identity::from_nsec(nsec(identities, name)).unwrap())
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

fn open(origin: &RelayOrigin, identity: Arc<Identity>) -> Session {
    Session::open(
        &tokio::runtime::Handle::current(),
        origin.clone(),
        identity,
        Store::in_memory().unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn presence_typing_and_mentions_reach_another_session() {
    let Some((origin, identities, seed)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let core = seed["channels"]["core-team"].as_str().unwrap().to_string();
    let (alice_id, bob_id) = (identity(&identities, "alice"), identity(&identities, "bob"));
    let (alice_pk, bob_pk) = (alice_id.pubkey_hex(), bob_id.pubkey_hex());

    let alice = open(&origin, alice_id);
    let bob = open(&origin, bob_id.clone());
    let mut incoming = alice.incoming();
    alice.watch_presence(vec![bob_pk.clone(), alice_pk.clone()]);
    let mut snapshot = alice.snapshot();
    until(&mut snapshot, "alice's roster", |s| {
        s.roster == RosterStatus::Ready && s.live
    })
    .await;

    // Bob publishes online on connect; alice reads it by snapshot or live,
    // and sees her own accepted publication.
    until(&mut snapshot, "bob online", |s| {
        s.presence.get(&bob_pk) == Some(&PresenceState::Online)
    })
    .await;
    until(&mut snapshot, "alice online", |s| {
        s.presence.get(&alice_pk) == Some(&PresenceState::Online)
    })
    .await;

    // Idle turns bob away; the live route carries the change.
    bob.set_idle(true);
    until(&mut snapshot, "bob away", |s| {
        s.presence.get(&bob_pk) == Some(&PresenceState::Away)
    })
    .await;

    // An agent-style typing pulse from bob shows in #core-team.
    let keys = Keys::parse(nsec(&identities, "bob")).unwrap();
    let socket = RelaySocket::connect(
        &tokio::runtime::Handle::current(),
        origin.clone(),
        bob_id.clone(),
        Arc::new(Verifier::default()),
        SocketOptions::default(),
    );
    let mut state = socket.state();
    tokio::time::timeout(
        Duration::from_secs(10),
        state.wait_for(|s| *s == SocketState::Ready),
    )
    .await
    .expect("bob's socket authenticates")
    .unwrap();
    let pulse = EventBuilder::new(Kind::from_u16(kinds::TYPING), "")
        .tags([Tag::custom("h", [core.as_str()])])
        .finalize(&keys)
        .unwrap();
    socket.publish(pulse).await.unwrap();
    until(&mut snapshot, "bob typing", |s| {
        s.typing.get(&core).is_some_and(|who| who.contains(&bob_pk))
    })
    .await;

    // Bob's message mentioning alice ends the pulse and notifies alice.
    let content = format!("presence test ping {}", std::process::id());
    let unsigned = bob_id
        .template(
            Purpose::Content,
            kinds::CHAT,
            Timestamp::now().as_secs(),
            vec![
                vec!["h".into(), core.clone()],
                vec!["p".into(), alice_pk.clone()],
            ],
            content.clone(),
        )
        .unwrap();
    let message = bob_id.sign(Purpose::Content, unsigned).unwrap();
    RelayHttp::new(origin.clone(), bob_id.clone())
        .post("/events", serde_json::to_string(&message).unwrap())
        .await
        .unwrap();
    until(&mut snapshot, "bob's typing to end", |s| {
        !s.typing.get(&core).is_some_and(|who| who.contains(&bob_pk))
    })
    .await;
    let arrived = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let item = incoming.recv().await.unwrap();
            if item.message_id == message.id {
                return item;
            }
        }
    })
    .await
    .expect("the mention notifies alice");
    assert_eq!(arrived.category, Category::Mention);
    assert!(arrived.mentioned);
    assert_eq!(arrived.channel_id, core);
    assert_eq!(arrived.author, bob_pk);
    assert_eq!(arrived.preview, content);
    assert_eq!(arrived.audience, None);

    // A mention between agents still arrives, saying who it is for: the
    // app does not alert people of it.
    let unsigned = bob_id
        .template(
            Purpose::Content,
            kinds::CHAT,
            Timestamp::now().as_secs(),
            vec![
                vec!["h".into(), core.clone()],
                vec!["p".into(), alice_pk.clone()],
                vec!["audience".into(), "agents".into()],
            ],
            format!("{content} for agents"),
        )
        .unwrap();
    let quiet = bob_id.sign(Purpose::Content, unsigned).unwrap();
    RelayHttp::new(origin.clone(), bob_id)
        .post("/events", serde_json::to_string(&quiet).unwrap())
        .await
        .unwrap();
    let arrived = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let item = incoming.recv().await.unwrap();
            if item.message_id == quiet.id {
                return item;
            }
        }
    })
    .await
    .expect("the agents' mention arrives");
    assert_eq!(arrived.audience, Some(Audience::Agents));
}
