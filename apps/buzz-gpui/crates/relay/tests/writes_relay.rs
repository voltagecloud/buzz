//! Message writes against the local test relay, in a throwaway channel.
//! Skipped unless `BUZZ_TEST_RELAY` points at `.test-relay`.

use buzz_identity::Identity;
use buzz_relay_client::lifecycle::NewChannel;
use buzz_relay_client::messages::ReportType;
use buzz_relay_client::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

struct Fixture {
    origin: RelayOrigin,
    identities: Value,
}

fn fixture() -> Option<Fixture> {
    let dir = std::env::var("BUZZ_TEST_RELAY").ok()?;
    let identities: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/identities.json")).unwrap())
            .unwrap();
    let origin = RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap();
    Some(Fixture { origin, identities })
}

impl Fixture {
    async fn session(&self, name: &str) -> Session {
        let nsec = self.identities["identities"][name]["nsec"]
            .as_str()
            .unwrap();
        let session = Session::open(
            &tokio::runtime::Handle::current(),
            self.origin.clone(),
            Arc::new(Identity::from_nsec(nsec).unwrap()),
            Store::in_memory().unwrap(),
        );
        let mut snapshot = session.snapshot();
        until(&mut snapshot, "the roster and live updates", |s| {
            s.roster == RosterStatus::Ready && s.live
        })
        .await;
        session
    }
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

/// The row with this content once the relay echoed it.
async fn echoed(view: &ChannelView, content: &str) -> Arc<fold::MessageRow> {
    let mut snapshot = view.snapshot.clone();
    let seen = until(&mut snapshot, content, |s| {
        s.rows.iter().any(|r| {
            r.content == content
                && r.delivery
                    .as_ref()
                    .is_none_or(|d| d.status == fold::Delivery::Seen)
        })
    })
    .await;
    seen.rows
        .iter()
        .find(|r| r.content == content)
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_follow_participation_provenance_and_reports() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let tag = std::process::id();
    let alice = fixture.session("alice").await;
    let channel = alice
        .create_channel(NewChannel {
            name: format!("#writes-{tag}"),
            description: "Created by a test".into(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    let view = alice.open_channel(&channel);

    // A send shows at once, before the relay answers.
    let first = format!("first {tag}");
    alice
        .send(&channel, &first, Mentions::default(), Vec::new(), None)
        .await
        .unwrap();
    let mut shown = view.snapshot.clone();
    until(&mut shown, "the optimistic row", |s| {
        s.rows.iter().any(|r| r.content == first)
    })
    .await;
    let root = echoed(&view, &first).await;
    // An earlier first message confirms from the outbox; another
    // session, without it, confirms from the relay.
    alice.dm_delivered(root.id, &channel).await.unwrap();
    let bob = fixture.session("bob").await;
    bob.dm_delivered(root.id, &channel).await.unwrap_err();
    let unknown = nostr::prelude::EventId::from_byte_array([7; 32]);
    assert_eq!(
        alice.dm_delivered(unknown, &channel).await,
        Err("The earlier send could not be confirmed. Reconnect and retry.".into())
    );
    drop(bob);

    // Edits keep the folded media provenance.
    alice
        .edit(&channel, root.id, &format!("first, edited {tag}"))
        .await
        .unwrap();
    let mut snapshot = view.snapshot.clone();
    until(&mut snapshot, "the edit", |s| {
        s.rows.iter().any(|r| r.id == root.id && r.edited)
    })
    .await;

    // A thread reply copied to the channel names its thread.
    let reply = format!("reply {tag}");
    alice
        .send(
            &channel,
            &reply,
            Mentions::default(),
            Vec::new(),
            Some((root.id, root.id)),
        )
        .await
        .unwrap();
    let thread = alice.open_thread(&channel, root.id, false);
    let mut replies = thread.snapshot.clone();
    let loaded = until(&mut replies, "the reply", |s| {
        s.replies.iter().any(|r| {
            r.content == reply
                && r.delivery
                    .as_ref()
                    .is_none_or(|d| d.status == fold::Delivery::Seen)
        })
    })
    .await;
    let row = loaded
        .replies
        .iter()
        .find(|r| r.content == reply)
        .unwrap()
        .clone();
    alice
        .send_to_channel(row, loaded.root.clone())
        .await
        .unwrap();
    let copy = echoed(&view, &reply).await;
    let thread_ref = copy.sent_from_thread.as_ref().expect("provenance");
    assert_eq!(thread_ref.root_id, root.id);
    assert_ne!(copy.id, root.id);

    // Reports go straight to the relay.
    alice
        .report(&channel, root.id, ReportType::Spam, "test report")
        .await
        .unwrap();

    // Someone who has not joined may read a public channel but not write.
    let bob = fixture.session("bob").await;
    // A channel bob has not joined is a preview once resolved.
    let resolved = bob.resolve_channels(vec![channel.clone()]).await.unwrap();
    assert!(resolved.iter().any(|c| c.id == channel && c.read_only));
    let preview = bob.open_channel(&channel);
    bob.select(Some(channel.clone()));
    let mut shown = preview.snapshot.clone();
    until(&mut shown, "bob's preview", |s| {
        s.rows.iter().any(|r| r.id == root.id)
    })
    .await;
    assert_eq!(
        bob.react(&channel, root.id, "👍").await,
        Err("Join the conversation before posting".into())
    );

    // Removing a message that is not ours or an owned agent's is refused.
    assert!(bob.remove(&channel, root.id).await.is_err());
    alice.remove(&channel, root.id).await.unwrap();
    until(&mut snapshot, "the removal", |s| {
        s.rows.iter().all(|r| r.id != root.id)
    })
    .await;
}
