//! A whole community session against the local test relay. Skipped unless
//! `BUZZ_TEST_RELAY` points at `.test-relay`.

use buzz_identity::{Identity, Purpose};
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

#[tokio::test(flavor = "multi_thread")]
async fn lists_channels_and_folds_a_timeline() {
    let Some((origin, identities, seed)) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = Arc::new(
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let session = Session::open(
        &tokio::runtime::Handle::current(),
        origin,
        alice,
        Store::in_memory().unwrap(),
    );
    let mut snapshot = session.snapshot();
    let channel = |name: &str| seed["channels"][name].as_str().unwrap().to_string();

    let ready = until(&mut snapshot, "the roster", |s| {
        s.roster == RosterStatus::Ready && !s.channels.is_empty()
    })
    .await;
    let ids: Vec<&str> = ready.channels.iter().map(|c| c.id.as_str()).collect();
    for name in ["general", "random", "core-team", "dm-alice-bob"] {
        assert!(
            ids.contains(&channel(name).as_str()),
            "{name} missing from {ids:?}"
        );
    }
    let dm = ready
        .channels
        .iter()
        .find(|c| c.id == channel("dm-alice-bob"))
        .unwrap();
    assert_eq!(dm.channel_type, Some(ChannelType::Dm));
    assert_eq!(ready.relay.as_ref().unwrap().authority.len(), 64);
    // The unread sample also yields each channel's newest message time.
    let sampled = until(&mut snapshot, "message times", |s| {
        s.unread.latest.contains_key(&channel("general"))
            && s.unread.latest.contains_key(&channel("dm-alice-bob"))
    })
    .await;
    assert!(sampled.unread.latest[&channel("general")] > 1_600_000_000);

    let view = session.open_channel(&channel("general"));
    let mut timeline = view.snapshot.clone();
    let welcome = seed["events"]["welcome"].as_str().unwrap();
    // Other tests post to #general too, so page back to the seeded
    // welcome rather than expecting it on the first page.
    let mut shown = 0;
    let loaded = loop {
        let page = until(&mut timeline, "a #general page", |t| {
            t.status == TimelineStatus::Ready
                && !t.loading_older
                && (t.rows.len() > shown || !t.has_more)
        })
        .await;
        if page.rows.iter().any(|r| r.id.to_hex() == welcome) {
            break page;
        }
        assert!(page.has_more, "the welcome is not in #general");
        shown = page.rows.len();
        view.load_older();
    };
    let row = loaded
        .rows
        .iter()
        .find(|r| r.id.to_hex() == welcome)
        .unwrap();
    // At least the seeded replies; other runs may have added more.
    assert!(row.reply_count >= 2, "thread summary folded in");
    assert!(!row.reactions.is_empty(), "reactions folded in");
    // Replies stay in the thread, not the channel timeline.
    let reply = seed["events"]["thread_reply_bob"].as_str().unwrap();
    assert!(loaded.rows.iter().all(|r| r.id.to_hex() != reply));

    let bob = identities["identities"]["bob"]["pk_hex"]
        .as_str()
        .unwrap()
        .to_string();
    let profiles = until(&mut snapshot, "bob's profile", |s| {
        s.profiles.contains_key(&bob)
    })
    .await;
    assert!(!profiles.profiles[&bob].name_is_fallback);
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_reacts_and_deletes_through_the_outbox() {
    let Some((origin, identities, seed)) = fixture() else {
        return;
    };
    let alice = Arc::new(
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let session = Session::open(
        &tokio::runtime::Handle::current(),
        origin,
        alice,
        Store::in_memory().unwrap(),
    );
    let mut snapshot = session.snapshot();
    until(&mut snapshot, "the roster", |s| {
        s.roster == RosterStatus::Ready && s.live
    })
    .await;
    let random = seed["channels"]["random"].as_str().unwrap().to_string();
    let view = session.open_channel(&random);
    let mut timeline = view.snapshot.clone();
    until(&mut timeline, "#random", |t| {
        t.status == TimelineStatus::Ready
    })
    .await;

    let content = format!("outbox test {}", std::process::id());
    session
        .send(&random, &content, Default::default(), Vec::new(), None)
        .await
        .unwrap();
    let seen = until(&mut timeline, "the echo of our message", |t| {
        t.rows.iter().any(|r| {
            r.content == content
                && r.delivery
                    .as_ref()
                    .is_none_or(|d| d.status == fold::Delivery::Seen)
        })
    })
    .await;
    let id = seen.rows.iter().find(|r| r.content == content).unwrap().id;

    session.react(&random, id, "👍").await.unwrap();
    until(&mut timeline, "our reaction", |t| {
        t.rows
            .iter()
            .any(|r| r.id == id && r.reactions.iter().any(|g| g.content == "👍"))
    })
    .await;

    session.remove(&random, id).await.unwrap();
    until(&mut timeline, "the deletion", |t| {
        t.rows.iter().all(|r| r.id != id)
    })
    .await;
}

fn identity(identities: &Value, name: &str) -> Arc<Identity> {
    Arc::new(Identity::from_nsec(identities["identities"][name]["nsec"].as_str().unwrap()).unwrap())
}

/// Posts a top-level message as `name` over HTTP.
async fn post(origin: &RelayOrigin, identities: &Value, name: &str, channel: &str, content: &str) {
    let author = identity(identities, name);
    let unsigned = author
        .template(
            Purpose::Content,
            9,
            nostr::prelude::Timestamp::now().as_secs(),
            vec![vec!["h".into(), channel.to_string()]],
            content.to_string(),
        )
        .unwrap();
    let event = author.sign(Purpose::Content, unsigned).unwrap();
    RelayHttp::new(origin.clone(), author)
        .post("/events", serde_json::to_string(&event).unwrap())
        .await
        .unwrap();
}

fn open_as_alice(origin: &RelayOrigin, identities: &Value, store: Store) -> Session {
    Session::open(
        &tokio::runtime::Handle::current(),
        origin.clone(),
        identity(identities, "alice"),
        store,
    )
}

/// Read-state frontiers are whole seconds and a read covers messages in
/// its own second, so a message meant to be new waits for the next one.
async fn next_second() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

// Read state syncs between alice's sessions, so the tests below use
// channels no other test reads: #general here, the DM below.
#[tokio::test(flavor = "multi_thread")]
async fn marking_a_channel_read_persists_and_syncs() {
    let Some((origin, identities, seed)) = fixture() else {
        return;
    };
    let general = seed["channels"]["general"].as_str().unwrap().to_string();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("buzz.sqlite3");

    let session = open_as_alice(&origin, &identities, Store::open(&path).unwrap());
    let mut snapshot = session.snapshot();
    until(&mut snapshot, "the unread sample", |s| {
        s.roster == RosterStatus::Ready && s.unread.observed
    })
    .await;
    session.mark_channel_read(&general).await.unwrap();
    until(&mut snapshot, "#general read", |s| {
        !s.unread.channel(&general).unread
    })
    .await;

    next_second().await;
    let content = format!("unread test {}", std::process::id());
    post(&origin, &identities, "bob", &general, &content).await;
    let unread = until(&mut snapshot, "#general unread", |s| {
        s.unread.channel(&general).unread
    })
    .await;
    let row = unread.unread.channel(&general);
    assert!(row.count >= 1 && row.bold, "{row:?}");

    session.mark_channel_read(&general).await.unwrap();
    until(&mut snapshot, "#general read again", |s| {
        !s.unread.channel(&general).unread
    })
    .await;
    // The read reaches the relay after the publish debounce.
    until(&mut snapshot, "the read published", |s| {
        s.unread
            .sync
            .as_ref()
            .is_some_and(|sync| sync.status == ReadSyncStatus::Reconciled)
    })
    .await;
    drop(session);

    // The same device remembers it from its store...
    let session = open_as_alice(&origin, &identities, Store::open(&path).unwrap());
    let mut snapshot = session.snapshot();
    let reopened = until(&mut snapshot, "the reopened sample", |s| {
        s.roster == RosterStatus::Ready && s.unread.observed
    })
    .await;
    assert!(
        !reopened.unread.channel(&general).unread,
        "{:?}",
        reopened.unread
    );
    drop(session);

    // ...and a new device learns it from the relay.
    let session = open_as_alice(&origin, &identities, Store::in_memory().unwrap());
    let mut snapshot = session.snapshot();
    let synced = until(&mut snapshot, "a new device's sample", |s| {
        s.roster == RosterStatus::Ready && s.unread.observed
    })
    .await;
    assert!(
        !synced.unread.channel(&general).unread,
        "{:?}",
        synced.unread
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_arriving_while_reading_stay_read() {
    let Some((origin, identities, seed)) = fixture() else {
        return;
    };
    let dm = seed["channels"]["dm-alice-bob"]
        .as_str()
        .unwrap()
        .to_string();
    let session = open_as_alice(&origin, &identities, Store::in_memory().unwrap());
    let mut snapshot = session.snapshot();
    until(&mut snapshot, "the unread sample", |s| {
        s.roster == RosterStatus::Ready && s.unread.observed && s.live
    })
    .await;
    session.mark_channel_read(&dm).await.unwrap();

    let view = session.open_channel(&dm);
    session.select(Some(dm.clone()));
    let mut timeline = view.snapshot.clone();
    until(&mut timeline, "the DM", |t| {
        t.status == TimelineStatus::Ready
    })
    .await;
    // The view, in a focused window, sits at its newest message.
    view.viewport(Vec::new(), true);

    next_second().await;
    let content = format!("reading test {}", std::process::id());
    post(&origin, &identities, "bob", &dm, &content).await;
    until(&mut timeline, "bob's message", |t| {
        t.rows.iter().any(|r| r.content == content)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(READING_DWELL_MS + 700)).await;
    let reading = snapshot.borrow().unread.clone();
    assert!(
        !reading.channel(&dm).unread,
        "a message that arrived while reading is unread: {reading:?}"
    );

    // Leaving ends the reading: the next message counts, as a direct one.
    session.select(None);
    drop(view);
    next_second().await;
    let content = format!("after reading {}", std::process::id());
    post(&origin, &identities, "bob", &dm, &content).await;
    let left = until(&mut snapshot, "unread after leaving", |s| {
        s.unread.channel(&dm).unread
    })
    .await;
    let row = left.unread.channel(&dm);
    assert!(row.priority && left.unread.direct >= 1, "{:?}", left.unread);
}

#[tokio::test(flavor = "multi_thread")]
async fn opens_a_thread_with_its_replies() {
    let Some((origin, identities, seed)) = fixture() else {
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
    let mut snapshot = session.snapshot();
    until(&mut snapshot, "the roster", |s| {
        s.roster == RosterStatus::Ready
    })
    .await;
    let general = seed["channels"]["general"].as_str().unwrap();
    let root: nostr::prelude::EventId =
        seed["events"]["welcome"].as_str().unwrap().parse().unwrap();
    let view = session.open_thread(general, root, false);
    let mut thread = view.snapshot.clone();
    let loaded = until(&mut thread, "the thread", |t| {
        t.root.is_some() && t.replies.len() >= 2
    })
    .await;
    assert_eq!(loaded.root.as_ref().unwrap().id, root);
    let replies: Vec<String> = loaded.replies.iter().map(|r| r.id.to_hex()).collect();
    for name in ["thread_reply_bob", "thread_reply_carol"] {
        assert!(replies.contains(&seed["events"][name].as_str().unwrap().to_string()));
    }

    // Selecting a reply keeps the thread open on its rows; a message of
    // another thread is left for a new view.
    let event = |name: &str| -> nostr::prelude::EventId {
        seed["events"][name].as_str().unwrap().parse().unwrap()
    };
    let reply = event("thread_reply_bob");
    let outcome = view.retarget(reply).await.unwrap();
    assert_eq!(outcome, Retarget::Selected);
    let selected = thread.borrow().clone();
    assert_eq!(selected.target.as_ref().unwrap().id, reply);
    assert_eq!(selected.replies.len(), loaded.replies.len());
    let elsewhere = view.retarget(event("bob_hi")).await.unwrap();
    assert_eq!(elsewhere, Retarget::Different);
    assert_eq!(thread.borrow().target.as_ref().unwrap().id, reply);
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_an_image_and_sends_it_with_a_mention() {
    let Some((origin, identities, seed)) = fixture() else {
        return;
    };
    let alice = Arc::new(
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let bob = identities["identities"]["bob"]["pk_hex"]
        .as_str()
        .unwrap()
        .to_string();
    let session = Session::open(
        &tokio::runtime::Handle::current(),
        origin,
        alice,
        Store::in_memory().unwrap(),
    );
    // The catalog loads only once something asks for it.
    session.ensure_emoji();
    let mut snapshot = session.snapshot();
    let ready = until(&mut snapshot, "the roster and emoji", |s| {
        s.roster == RosterStatus::Ready && s.live && s.emoji.status == CatalogStatus::Ready
    })
    .await;
    let random = seed["channels"]["random"].as_str().unwrap().to_string();
    assert!(
        ready
            .channels
            .iter()
            .any(|c| c.id == random && c.members.contains(&bob))
    );
    let view = session.open_channel(&random);
    let mut timeline = view.snapshot.clone();
    until(&mut timeline, "#random", |t| {
        t.status == TimelineStatus::Ready
    })
    .await;

    // A unique image each run, so the relay stores a new blob.
    let shade = (std::process::id() % 250) as u8;
    let image = image::RgbImage::from_pixel(8, 8, image::Rgb([shade, 120, 40]));
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut bytes)
        .encode_image(&image)
        .unwrap();
    let attachment = upload::upload(
        session.media(),
        UploadFile {
            name: "square.jpg".into(),
            mime: "image/jpeg".into(),
            bytes,
        },
    )
    .await
    .unwrap();
    assert_eq!(attachment.mime, "image/jpeg");
    assert!(session.media().is_relay_media(&attachment.url));

    let content = format!("@Bob attachment test {}", std::process::id());
    let mentions = Mentions {
        recipients: vec![bob.clone()],
        references: Vec::new(),
        ..Default::default()
    };
    session
        .send(&random, &content, mentions, vec![attachment.clone()], None)
        .await
        .unwrap();
    // The relay checks each `imeta` against the stored blob, so an accepted
    // send proves the descriptor. Attachments only render over HTTPS, which
    // the plain test relay is not.
    let seen = until(&mut timeline, "the accepted echo", |t| {
        t.rows.iter().any(|r| {
            r.content == content
                && r.delivery
                    .as_ref()
                    .is_some_and(|d| d.status == fold::Delivery::Seen)
        })
    })
    .await;
    let row = seen.rows.iter().find(|r| r.content == content).unwrap();
    assert_eq!(
        row.mentions.iter().map(|k| k.to_hex()).collect::<Vec<_>>(),
        [bob]
    );
}
