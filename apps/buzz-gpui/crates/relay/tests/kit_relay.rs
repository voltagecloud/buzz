//! Channel templates against the local test relay: saving, reading,
//! a revision conflict and deleting a throwaway template, a portable
//! team's payload, and creating a channel with setup. Skipped unless
//! `BUZZ_TEST_RELAY` points at `.test-relay`. Every record is deleted and
//! every channel removed afterwards.

use buzz_identity::Identity;
use buzz_relay_client::kit::setup::{ChannelCreationInput, ChannelSetup};
use buzz_relay_client::kit::{KitStatus, KitValue, Template};
use buzz_relay_client::lifecycle::{self, Action, NewChannel};
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
    Some(Fixture {
        origin: RelayOrigin::parse(identities["relay_ws_url"].as_str().unwrap()).unwrap(),
        identities,
    })
}

impl Fixture {
    fn pubkey(&self, name: &str) -> String {
        self.identities["identities"][name]["pk_hex"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn session(&self, name: &str, store: Store) -> Arc<Session> {
        let nsec = self.identities["identities"][name]["nsec"]
            .as_str()
            .unwrap();
        let session = Session::open(
            &tokio::runtime::Handle::current(),
            self.origin.clone(),
            Arc::new(Identity::from_nsec(nsec).unwrap()),
            store,
        );
        let mut snapshot = session.snapshot();
        until(&mut snapshot, "the roster", |s| {
            s.roster == RosterStatus::Ready && s.live
        })
        .await;
        Arc::new(session)
    }
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

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
}

fn entry(session: &Session, id: &str) -> Option<kit::KitEntry> {
    session
        .kit()
        .borrow()
        .entries
        .iter()
        .find(|e| e.record.value.id() == id)
        .cloned()
}

/// Replaceable records use seconds: the next save waits a second.
async fn next_second() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn saves_reads_conflicts_and_deletes_a_template() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let carol = fixture.session("carol", Store::in_memory().unwrap()).await;
    carol.kit_refresh().await.unwrap();
    assert_eq!(carol.kit().borrow().status, KitStatus::Ready);

    let mut template = Template::blank();
    template.name = unique("template");
    template.description = "Created by a test".into();
    template.agents = vec![fixture.pubkey("bob")];
    template.canvas = "# Goal\n\nShip it".into();
    let first = carol
        .kit_save(KitValue::Template(template.clone()), None, false)
        .await
        .unwrap();
    let saved = entry(&carol, &template.id).expect("the saved template is listed");
    assert_eq!(saved.event_id, first);
    assert_eq!(saved.record.value, KitValue::Template(template.clone()));

    // A fresh session reads and decrypts it.
    let other = fixture.session("carol", Store::in_memory().unwrap()).await;
    other.kit_refresh().await.unwrap();
    assert_eq!(entry(&other, &template.id).unwrap().event_id, first);

    // An edit over the head it was opened from succeeds…
    next_second().await;
    let mut edited = template.clone();
    edited.name.push_str(" (edited)");
    let second = carol
        .kit_save(KitValue::Template(edited.clone()), Some(first), false)
        .await
        .unwrap();
    // …and an edit from the stale head is refused.
    next_second().await;
    let stale = other
        .kit_save(KitValue::Template(template.clone()), Some(first), false)
        .await
        .unwrap_err();
    assert!(stale.starts_with("This saved recipe changed"), "{stale}");
    // So is a second save within the same second as the head.
    let same_second = carol
        .kit_save(KitValue::Template(edited.clone()), Some(second), false)
        .await;
    if let Err(error) = &same_second {
        assert!(
            error.starts_with("Please wait a second"),
            "only the same-second rule may refuse: {error}"
        );
    }
    let head = same_second.unwrap_or(second);

    // Deleting saves the record as deleted; it leaves the list.
    next_second().await;
    carol
        .kit_save(KitValue::Template(edited), Some(head), true)
        .await
        .unwrap();
    let deleted = entry(&carol, &template.id).expect("the tombstone is read");
    assert!(deleted.record.deleted);
    assert!(
        kit::templates(&carol.kit().borrow().entries)
            .iter()
            .all(|t| t.id != template.id)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn creates_a_channel_with_its_setup_and_retires_the_receipt() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let store = Store::in_memory().unwrap();
    let creator = fixture.session("carol", store.clone()).await;
    let bob = fixture.pubkey("bob");
    let name = unique("setup");
    let input = ChannelCreationInput::from_new(
        &NewChannel {
            name: format!("#{name}"),
            description: "Created by a test".into(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        },
        Some(ChannelSetup {
            agents: vec![bob.clone()],
            template_id: "test-template".into(),
            ..ChannelSetup::default()
        }),
    );
    let id = creator.create_channel_with(input).await.unwrap();
    assert!(
        creator
            .snapshot()
            .borrow()
            .channels
            .iter()
            .any(|c| c.id == id),
        "admitted once the viewer is in the roster"
    );
    // Completion adds the agent, then retires the receipt.
    let mut snapshot = creator.snapshot();
    until(&mut snapshot, "the agent in the roster", |s| {
        s.channels
            .iter()
            .any(|c| c.id == id && c.members.contains(&bob))
    })
    .await;
    let partition = store::Partition::new(&fixture.origin, creator.viewer());
    for _ in 0..50 {
        if kit::setup::Receipts::load(&store, &partition)
            .unwrap()
            .0
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        kit::setup::Receipts::load(&store, &partition)
            .unwrap()
            .0
            .is_empty()
    );
    assert!(creator.creation().borrow().notices.is_empty());
    assert_eq!(creator.pending_creation().await, None);

    // A starting Canvas is seeded through the outbox, then confirmed as
    // the selected document.
    let canvas = ChannelCreationInput::from_new(
        &NewChannel {
            name: unique("canvas"),
            description: String::new(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        },
        Some(ChannelSetup {
            canvas: "# Goal".into(),
            ..ChannelSetup::default()
        }),
    );
    let seeded = creator.create_channel_with(canvas).await.unwrap();
    let mut head = None;
    for _ in 0..50 {
        head = creator.canvas_read(&seeded, true).await.unwrap();
        if head.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(head.expect("the seeded Canvas").content, "# Goal");
    creator
        .channel_action(Action::Delete, &seeded)
        .await
        .unwrap();

    creator.channel_action(Action::Delete, &id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_a_creation_restored_after_a_restart() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    use buzz_identity::Purpose;
    use buzz_relay_client::kit::setup::{Progress, Receipts};
    let nsec = fixture.identities["identities"]["carol"]["nsec"]
        .as_str()
        .unwrap();
    let creator = Identity::from_nsec(nsec).unwrap();
    // The previous run saved its intent and its Create, then stopped
    // before the relay answered.
    let store = Store::in_memory().unwrap();
    let partition = Partition::new(&fixture.origin, &creator.pubkey_hex());
    let channel = NewChannel {
        name: unique("restored"),
        description: String::new(),
        private: false,
        ttl: None,
        labels: Vec::new(),
    };
    let mut progress = Progress::new(ChannelCreationInput::from_new(
        &channel,
        Some(ChannelSetup {
            template_id: "test-template".into(),
            ..ChannelSetup::default()
        }),
    ));
    let draft = lifecycle::create(&channel, &progress.id).unwrap();
    let mut tags = draft.tags;
    tags.push(vec!["client-id".into(), "restart-test".into()]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let event = creator
        .template(Purpose::Content, draft.kind, now, tags, String::new())
        .unwrap();
    progress
        .operations
        .insert("create".into(), event.id.unwrap().to_hex());
    Receipts::write(&store, &partition, &progress).unwrap();
    SqliteJournal::new(store.clone(), partition.clone())
        .commit(vec![Operation {
            event,
            signed: None,
            recovery: None,
            acknowledged: false,
            guarded: false,
            delivery: Delivery::Sending,
            error: None,
        }])
        .await
        .unwrap();

    let session = fixture.session("carol", store.clone()).await;
    let pending = session.pending_creation().await.expect("restored");
    assert_eq!(pending.name, channel.name);
    // "Retry channel" sends only that Create; the setup is reported, not
    // continued.
    let id = session.create_channel_with(pending).await.unwrap();
    assert_eq!(id, progress.id);
    let mut creation = session.creation();
    let snapshot = until(&mut creation, "the setup notice", |c| !c.notices.is_empty()).await;
    assert_eq!(snapshot.notices[0].id, id);
    assert!(snapshot.notices[0].error.starts_with("Channel confirmed."));
    assert_eq!(session.pending_creation().await, None);
    session.dismiss_setup_notice(&id);
    assert!(session.creation().borrow().notices.is_empty());
    session.channel_action(Action::Delete, &id).await.unwrap();
}
