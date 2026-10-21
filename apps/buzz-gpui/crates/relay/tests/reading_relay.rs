//! Reading and explicit unread marks against the local test relay: a view
//! reads nothing until it reports what it shows, a thread panel reads its
//! own thread, and the unread watch follows rows a view shows. Skipped
//! unless `BUZZ_TEST_RELAY` points at `.test-relay`. Messages go to a
//! throwaway channel whose name carries the process id.

use buzz_identity::Identity;
use buzz_relay_client::lifecycle::{Action, NewChannel};
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
        until(&mut snapshot, "the unread sample", |s| {
            s.roster == RosterStatus::Ready && s.live && s.unread.observed
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
        // The startup unread sample reads every channel, which takes a while
        // on a busy test relay.
        Duration::from_secs(60),
        receiver.wait_for(|value| predicate(value)),
    )
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    .unwrap()
    .clone()
}

/// Read-state frontiers are whole seconds and a read covers messages in
/// its own second, so a message meant to be new waits for the next one.
async fn next_second() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

/// A public channel alice owns and bob has joined.
async fn shared_channel(alice: &Session, bob: &Session, name: &str) -> String {
    let id = alice
        .create_channel(NewChannel {
            name: format!("#{name}-{}", std::process::id()),
            description: "Created by a test".into(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    bob.channel_action(Action::Join, &id).await.unwrap();
    for session in [alice, bob] {
        let mut snapshot = session.snapshot();
        until(&mut snapshot, "the shared channel", |s| {
            s.channels.iter().any(|c| c.id == id && !c.read_only)
        })
        .await;
    }
    id
}

async fn send(session: &Session, channel: &str, content: &str, thread: Option<EventId>) {
    session
        .send(
            channel,
            content,
            Mentions::default(),
            Vec::new(),
            thread.map(|root| (root, root)),
        )
        .await
        .unwrap();
}

async fn row(view: &ChannelView, content: &str) -> Arc<fold::MessageRow> {
    let mut timeline = view.snapshot.clone();
    let seen = until(&mut timeline, content, |t| {
        t.rows.iter().any(|r| r.content == content)
    })
    .await;
    seen.rows
        .iter()
        .find(|r| r.content == content)
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_only_what_the_view_reports_and_marks_by_hand() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = fixture.session("alice").await;
    let bob = fixture.session("bob").await;
    let id = shared_channel(&alice, &bob, "reading").await;
    let mut snapshot = alice.snapshot();

    // Selected and open, but the view has not said what it shows: a
    // hidden or unfocused window reads nothing.
    let view = alice.open_channel(&id);
    alice.select(Some(id.clone()));
    let mut timeline = view.snapshot.clone();
    until(&mut timeline, "the channel", |t| {
        t.status == TimelineStatus::Ready
    })
    .await;
    next_second().await;
    let content = format!("unseen {}", std::process::id());
    send(&bob, &id, &content, None).await;
    row(&view, &content).await;
    let unread = until(&mut snapshot, "the message unread", |s| {
        s.unread.channel(&id).unread
    })
    .await;
    tokio::time::sleep(Duration::from_millis(READING_DWELL_MS + 700)).await;
    assert!(
        snapshot.borrow().unread.channel(&id).unread,
        "{:?}",
        unread.unread
    );

    // At the bottom, the dwell catches the channel up.
    view.viewport(Vec::new(), true);
    until(&mut snapshot, "the channel read", |s| {
        !s.unread.channel(&id).unread
    })
    .await;

    // Unread on this device only, until it is read again.
    view.viewport(Vec::new(), false);
    alice.mark_channel_unread(&id).await.unwrap();
    let marked = until(&mut snapshot, "the local mark", |s| {
        s.unread.channel(&id).manual == Manual::LocalOnly
    })
    .await;
    assert!(marked.unread.channel(&id).unread);
    alice.mark_channel_read(&id).await.unwrap();
    until(&mut snapshot, "the mark cleared", |s| {
        let row = s.unread.channel(&id);
        !row.unread && row.manual == Manual::None
    })
    .await;

    drop(view);
    alice.channel_action(Action::Delete, &id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thread_panel_reads_its_own_thread() {
    let Some(fixture) = fixture() else {
        return;
    };
    let alice = fixture.session("alice").await;
    let bob = fixture.session("bob").await;
    let id = shared_channel(&alice, &bob, "thread-reading").await;
    let mut snapshot = alice.snapshot();

    let view = alice.open_channel(&id);
    let root_text = format!("root {}", std::process::id());
    send(&alice, &id, &root_text, None).await;
    let root = row(&view, &root_text).await.id;

    // Bob answers in alice's thread: a reply in her conversation.
    next_second().await;
    let reply_text = format!("reply {}", std::process::id());
    send(&bob, &id, &reply_text, Some(root)).await;
    let watch = alice.watch_unread(&id);
    let thread = alice.open_thread(&id, root, false);
    let mut panel = thread.snapshot.clone();
    let loaded = until(&mut panel, "the reply", |t| {
        t.replies.iter().any(|r| r.content == reply_text)
    })
    .await;
    let reply = loaded
        .replies
        .iter()
        .find(|r| r.content == reply_text)
        .unwrap()
        .id;
    watch.set(vec![root], vec![reply]);
    let mut detail = watch.snapshot.clone();
    until(&mut detail, "the reply unread", |d| {
        d.unread.contains(&reply) && d.threads.get(&root).and_then(|t| t.observed) == Some(1)
    })
    .await;
    until(&mut snapshot, "the unread thread", |s| {
        s.unread.channel(&id).threads == 1
            && s.unread.activity[&id]
                .items
                .as_ref()
                .is_some_and(|items| items[0].root_id == root.to_hex())
    })
    .await;

    // The panel shows the reply at its bottom: the thread is read.
    thread.viewport(vec![reply], true);
    until(&mut detail, "the thread read", |d| {
        !d.unread.contains(&reply) && d.threads.get(&root).and_then(|t| t.observed) == Some(0)
    })
    .await;
    until(&mut snapshot, "no unread thread", |s| {
        s.unread.channel(&id).threads == 0
    })
    .await;

    // A thread is read through a message by hand too.
    thread.viewport(Vec::new(), false);
    next_second().await;
    let again = format!("again {}", std::process::id());
    send(&bob, &id, &again, Some(root)).await;
    let loaded = until(&mut panel, "the second reply", |t| {
        t.replies.iter().any(|r| r.content == again)
    })
    .await;
    let second = loaded
        .replies
        .iter()
        .find(|r| r.content == again)
        .unwrap()
        .id;
    watch.set(vec![root], vec![reply, second]);
    until(&mut detail, "the second reply unread", |d| {
        d.unread.contains(&second)
    })
    .await;
    alice.mark_through(&id, Some(root), second).await.unwrap();
    until(&mut detail, "the thread read through", |d| {
        !d.unread.contains(&second)
    })
    .await;

    drop((thread, watch, view));
    alice.channel_action(Action::Delete, &id).await.unwrap();
}
