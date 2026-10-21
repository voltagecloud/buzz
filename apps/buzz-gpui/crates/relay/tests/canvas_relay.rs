//! Canvas saves, conflicts, history and restore against the local test
//! relay, in throwaway channels. Skipped unless `BUZZ_TEST_RELAY` points at
//! `.test-relay`.

use buzz_identity::{Identity, Purpose};
use buzz_relay_client::canvas::{self, HISTORY_PAGE};
use buzz_relay_client::lifecycle::NewChannel;
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
    fn identity(&self, name: &str) -> Arc<Identity> {
        let nsec = self.identities["identities"][name]["nsec"]
            .as_str()
            .unwrap();
        Arc::new(Identity::from_nsec(nsec).unwrap())
    }

    async fn session(&self, name: &str) -> Session {
        let session = Session::open(
            &tokio::runtime::Handle::current(),
            self.origin.clone(),
            self.identity(name),
            Store::in_memory().unwrap(),
        );
        let mut snapshot = session.snapshot();
        until(&mut snapshot, "the roster and live updates", |s| {
            s.roster == RosterStatus::Ready && s.live
        })
        .await;
        session
    }

    async fn channel(&self, session: &Session, what: &str) -> String {
        session
            .create_channel(NewChannel {
                name: format!("#canvas-{what}-{}", std::process::id()),
                description: "Created by a Canvas test".into(),
                private: false,
                ttl: None,
                labels: Vec::new(),
            })
            .await
            .unwrap()
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

/// Saves of one Canvas are spaced a second apart.
async fn next_second() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Posts a signed Canvas straight to the relay, bypassing the session.
async fn post(
    http: &RelayHttp,
    identity: &Identity,
    created_at: u64,
    tags: Vec<Vec<String>>,
    content: &str,
) -> Result<Vec<u8>, HttpError> {
    let unsigned = identity
        .template(
            Purpose::Content,
            40100,
            created_at,
            tags,
            content.to_owned(),
        )
        .unwrap();
    let event = identity.sign(Purpose::Content, unsigned).unwrap();
    http.post("/events", serde_json::to_string(&event).unwrap())
        .await
}

fn tags(channel: &str, expected: Option<&str>) -> Vec<Vec<String>> {
    let mut tags = vec![vec!["h".to_owned(), channel.to_owned()]];
    if let Some(expected) = expected {
        tags.push(vec!["expected-revision".to_owned(), expected.to_owned()]);
    }
    tags
}

#[tokio::test(flavor = "multi_thread")]
async fn saves_conflicts_history_and_restore() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = fixture.session("alice").await;
    let channel = fixture.channel(&alice, "save").await;
    assert!(alice.canvas_available().await);

    // A new channel has no Canvas and no history.
    assert_eq!(alice.canvas_read(&channel, true).await.unwrap(), None);
    let empty = alice.canvas_history(&channel, None).await.unwrap();
    assert!(empty.revisions.is_empty() && empty.next.is_none());

    // The first save claims there was none.
    let first = alice
        .canvas_save(&channel, "# Plan\n\n- first", None)
        .await
        .unwrap();
    assert_eq!(first.content, "# Plan\n\n- first");
    assert_eq!(first.author, alice.viewer());
    assert_eq!(
        alice.canvas_read(&channel, true).await.unwrap(),
        Some(first.clone())
    );

    // A draft from before that save is refused before anything is sent.
    assert_eq!(
        alice.canvas_save(&channel, "stale", None).await,
        Err(CanvasError::Conflict)
    );
    // Saving again within the head's second is refused.
    let soon = alice
        .canvas_save(&channel, "too soon", Some(first.id))
        .await;
    if first.created_at >= now() {
        assert_eq!(soon, Err(CanvasError::Failed(canvas::TOO_SOON.into())));
    }
    next_second().await;
    let second = match soon {
        Ok(saved) => saved,
        Err(_) => alice
            .canvas_save(&channel, "# Plan\n\n- second", Some(first.id))
            .await
            .unwrap(),
    };
    assert_ne!(second.id, first.id);

    // Over the limit never reaches the outbox.
    assert_eq!(
        alice
            .canvas_save(
                &channel,
                &"a".repeat(canvas::CANVAS_BYTES + 1),
                Some(second.id)
            )
            .await,
        Err(CanvasError::Failed(canvas::TOO_LARGE.into()))
    );

    // The relay itself refuses a stale precondition (compare-and-swap).
    next_second().await;
    let http = RelayHttp::new(fixture.origin.clone(), fixture.identity("alice"));
    let refused = post(
        &http,
        &fixture.identity("alice"),
        now(),
        tags(&channel, Some(&first.id.to_hex())),
        "stale",
    )
    .await;
    assert!(
        matches!(refused, Err(HttpError::Status { status: 409, .. })),
        "{refused:?}"
    );

    // Two writers racing from the same head: one wins, the other is told
    // the Canvas changed and its refused save leaves the outbox.
    let other = fixture.session("alice").await;
    let (a, b) = tokio::join!(
        alice.canvas_save(&channel, "from one", Some(second.id)),
        other.canvas_save(&channel, "from other", Some(second.id)),
    );
    let (winner, loser) = match (a, b) {
        (Ok(saved), Err(error)) => (saved, (error, &other)),
        (Err(error), Ok(saved)) => (saved, (error, &alice)),
        outcome => panic!("expected exactly one save to win: {outcome:?}"),
    };
    assert_eq!(loser.0, CanvasError::Conflict);
    next_second().await;
    let reviewed = loser
        .1
        .canvas_save(&channel, "reviewed", Some(winner.id))
        .await
        .unwrap();

    // History is newest first and marks nothing on its own; the head is
    // the reviewed save.
    let history = alice.canvas_history(&channel, None).await.unwrap();
    let ids: Vec<_> = history.revisions.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![reviewed.id, winner.id, second.id, first.id]);
    assert_eq!(history.next, None);

    // Restoring publishes the old content as a new revision on top of the
    // displayed head; history keeps everything.
    next_second().await;
    let restored = alice
        .canvas_save(&channel, &first.content, Some(reviewed.id))
        .await
        .unwrap();
    assert_eq!(restored.content, first.content);
    assert_ne!(restored.id, first.id);
    let after = alice.canvas_history(&channel, None).await.unwrap();
    assert_eq!(after.revisions.len(), 5);
    assert_eq!(after.revisions[0].id, restored.id);
    // Restoring with the base the history was opened at is a conflict.
    next_second().await;
    assert_eq!(
        alice
            .canvas_save(&channel, &second.content, Some(reviewed.id))
            .await,
        Err(CanvasError::Conflict)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn history_pages_by_twenty_five() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = fixture.session("alice").await;
    let channel = fixture.channel(&alice, "history").await;
    let identity = fixture.identity("alice");
    let http = RelayHttp::new(fixture.origin.clone(), identity.clone());

    // Older revisions, one per second, written without a precondition as
    // legacy clients do; two share a second to exercise the id tiebreak.
    let base = now() - 600;
    let mut times: Vec<u64> = (0..HISTORY_PAGE as u64 + 2).map(|i| base + i).collect();
    times.push(base + 5);
    for (i, at) in times.iter().enumerate() {
        post(
            &http,
            &identity,
            *at,
            tags(&channel, None),
            &format!("v{i}"),
        )
        .await
        .unwrap();
    }
    let first = alice.canvas_history(&channel, None).await.unwrap();
    assert_eq!(first.revisions.len(), HISTORY_PAGE);
    let next = first.next.expect("a second page");
    let second = alice.canvas_history(&channel, Some(next)).await.unwrap();
    assert_eq!(second.revisions.len(), times.len() - HISTORY_PAGE);
    assert_eq!(second.next, None);
    let all: Vec<_> = first.revisions.iter().chain(&second.revisions).collect();
    assert!(all.windows(2).all(|pair| {
        pair[0].created_at > pair[1].created_at
            || (pair[0].created_at == pair[1].created_at
                && pair[0].id.as_bytes() < pair[1].id.as_bytes())
    }));
    let unique: std::collections::HashSet<_> = all.iter().map(|r| r.id).collect();
    assert_eq!(unique.len(), times.len());
    // The head is the newest revision.
    let head = alice.canvas_read(&channel, true).await.unwrap().unwrap();
    assert_eq!(head.id, first.revisions[0].id);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_member_cannot_read_or_save() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = fixture.session("alice").await;
    let channel = fixture.channel(&alice, "access").await;
    let carol = fixture.session("carol").await;
    assert_eq!(
        carol.canvas_read(&channel, true).await,
        Err(CanvasError::Failed(canvas::UNAVAILABLE.into()))
    );
    assert_eq!(
        carol.canvas_save(&channel, "hi", None).await,
        Err(CanvasError::Failed(canvas::WRITE_UNAVAILABLE.into()))
    );
}
