//! Sharing a Me conversation (`sessions/share.ts`) against the local test
//! relay: moving it to Messages in place, and posting a link to it in
//! another channel after granting selected people or a new channel's
//! members access. Skipped unless `BUZZ_TEST_RELAY` points at
//! `.test-relay`. Every test signs in as throwaway members it invites
//! itself, so no seeded identity's records change.

use buzz_identity::Identity;
use buzz_relay_client::lifecycle::NewChannel;
use buzz_relay_client::session::Expected;
use buzz_relay_client::session::share::{
    Audience, Destination, ShareIntent, ShareOptions, Step, session_link_message,
};
use buzz_relay_client::socket::Filter;
use buzz_relay_client::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
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
    fn identity(&self, name: &str) -> Arc<Identity> {
        let nsec = self.identities["identities"][name]["nsec"]
            .as_str()
            .unwrap();
        Arc::new(Identity::from_nsec(nsec).unwrap())
    }

    /// A throwaway community member: claims a fresh invite from alice.
    async fn newcomer(&self) -> Arc<Identity> {
        let owner = RelayHttp::new(self.origin.clone(), self.identity("alice"));
        let minted = owner
            .post(
                "/api/invites",
                json!({"ttl_secs": 600, "max_uses": 1}).to_string(),
            )
            .await
            .unwrap();
        let minted: Value = serde_json::from_slice(&minted).unwrap();
        let identity = Arc::new(Identity::ephemeral());
        let http = RelayHttp::new(self.origin.clone(), identity.clone());
        let code = minted["code"].as_str().unwrap();
        community::claim(&http, code, None, false).await.unwrap();
        identity
    }

    async fn session(&self, identity: Arc<Identity>) -> Session {
        let session = Session::open(
            &tokio::runtime::Handle::current(),
            self.origin.clone(),
            identity,
            Store::in_memory().unwrap(),
        );
        let mut snapshot = session.snapshot();
        until(&mut snapshot, "the roster", |s| {
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

/// A confirmed Me conversation: a private work session placed in Me.
async fn me_conversation(session: &Session, title: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let op = session.work_session_create(&id, title, None).await.unwrap();
    session.work_session_delivered(op).await.unwrap();
    session
        .work_session_refresh(&id, Expected::default(), true)
        .await
        .unwrap();
    let kit = session.me_kit().clone();
    kit.refresh().await;
    kit.admit(&id, None).await.unwrap();
    kit.set_placement(&id, true, None).await.unwrap();
    assert!(kit.placed(&id));
    // Replaceable records keep second precision; moving it out again
    // needs a newer second.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    id
}

fn listed(session: &Session, id: &str) -> ChannelSummary {
    session
        .channels_now()
        .iter()
        .find(|c| c.id == id)
        .cloned()
        .unwrap()
}

fn selected(destination: Destination, people: Vec<String>) -> ShareIntent {
    ShareIntent {
        destination,
        audience: Audience::Selected,
        name: None,
        private: None,
        details: None,
        details_base: None,
        channel_people: Vec::new(),
        session_people: people,
    }
}

/// Runs the share and returns the steps it reported.
async fn share(session: &Session, source: &str, intent: ShareIntent) -> Vec<Step> {
    let attempt = session.begin_share(source, intent);
    assert!(Arc::ptr_eq(
        &attempt,
        &session.share_attempt(source).unwrap()
    ));
    let seen = Mutex::new(Vec::new());
    session
        .run_share(source, &attempt, &ShareOptions::default(), |step| {
            seen.lock().unwrap().push(step.clone())
        })
        .await
        .unwrap();
    // A finished share is forgotten; what it did stays.
    assert!(session.share_attempt(source).is_none());
    seen.into_inner().unwrap()
}

/// The viewer's link to `source` was posted in `destination`.
async fn link_posted(fixture: &Fixture, identity: Arc<Identity>, source: &str, destination: &str) {
    let http = RelayHttp::new(fixture.origin.clone(), identity.clone());
    let filter: Filter = Filter::from_iter([
        ("kinds".to_owned(), json!([9])),
        ("#h".to_owned(), json!([destination])),
        ("authors".to_owned(), json!([identity.pubkey_hex()])),
        ("limit".to_owned(), json!(10)),
    ]);
    let events = http.query(&[filter]).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.content == session_link_message(source)),
        "no link in {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn moves_a_conversation_to_messages() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let viewer = fixture.newcomer().await;
    let friend = fixture.newcomer().await.pubkey_hex();
    let session = fixture.session(viewer).await;
    let id = me_conversation(&session, "Plan the launch").await;
    assert_eq!(
        listed(&session, &id).channel_type,
        Some(ChannelType::Session)
    );

    // The dialog's draft: a session's marker description starts empty.
    let base = session.channel_details(&id).await.unwrap();
    assert!(base.can_edit);
    let mut draft = base.draft.clone();
    draft.name = "Launch plan".into();
    draft.description.clear();
    let intent = ShareIntent {
        details: Some(draft),
        details_base: Some(base),
        ..selected(Destination::Channel(id.clone()), vec![friend.clone()])
    };
    let steps = share(&session, &id, intent).await;
    assert_eq!(
        steps,
        [Step::ConfirmAccess, Step::SaveDetails, Step::MoveToMessages]
    );

    assert!(!session.me_kit().placed(&id));
    assert!(session.lists_member(&id, &friend).await.unwrap());
    let mut snapshot = session.snapshot();
    let moved = until(&mut snapshot, "the saved details", |s| {
        s.channels
            .iter()
            .any(|c| c.id == id && c.name == "Launch plan")
    })
    .await;
    let channel = moved.channels.iter().find(|c| c.id == id).unwrap();
    // Without its marker it is an ordinary private channel in Messages,
    // with its history.
    assert_eq!(channel.channel_type, Some(ChannelType::Stream));
    assert!(channel.private);
}

#[tokio::test(flavor = "multi_thread")]
async fn links_a_session_into_a_channel_for_selected_people() {
    let Some(fixture) = fixture() else {
        return;
    };
    let viewer = fixture.newcomer().await;
    let friend = fixture.newcomer().await.pubkey_hex();
    let session = fixture.session(viewer.clone()).await;
    let destination = session
        .create_channel(NewChannel {
            name: format!("share-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            description: String::new(),
            private: true,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    let source = me_conversation(&session, "Review").await;

    let steps = share(
        &session,
        &source,
        selected(
            Destination::Channel(destination.clone()),
            vec![friend.clone()],
        ),
    )
    .await;
    assert_eq!(steps, [Step::ConfirmAccess, Step::PostLink]);
    assert!(session.lists_member(&source, &friend).await.unwrap());
    // Access to the session only: the destination is unchanged, and the
    // session stays in Me.
    assert!(!session.lists_member(&destination, &friend).await.unwrap());
    assert!(session.me_kit().placed(&source));
    link_posted(&fixture, viewer, &source, &destination).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shares_with_everyone_in_a_new_channel() {
    let Some(fixture) = fixture() else {
        return;
    };
    let viewer = fixture.newcomer().await;
    let friend = fixture.newcomer().await.pubkey_hex();
    let session = fixture.session(viewer.clone()).await;
    let source = me_conversation(&session, "Kickoff").await;

    let intent = ShareIntent {
        audience: Audience::Everyone,
        name: Some(format!(
            "kickoff-{}",
            &uuid::Uuid::new_v4().to_string()[..8]
        )),
        private: Some(true),
        channel_people: vec![friend.clone()],
        ..selected(Destination::New, Vec::new())
    };
    let steps = share(&session, &source, intent).await;
    assert_eq!(
        steps,
        [
            Step::CreateChannel,
            Step::AddToNewChannel(friend.clone()),
            Step::CheckMembers,
            Step::ConfirmAccess,
            Step::PostLink,
        ]
    );
    let created = session
        .channels_now()
        .iter()
        .find(|c| c.name.starts_with("kickoff-") && c.id != source)
        .cloned()
        .unwrap();
    assert!(session.lists_member(&created.id, &friend).await.unwrap());
    // Everyone in the new channel can open the session.
    assert!(session.lists_member(&source, &friend).await.unwrap());
    link_posted(&fixture, viewer, &source, &created.id).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_share_keeps_its_intent() {
    let Some(fixture) = fixture() else {
        return;
    };
    let viewer = fixture.newcomer().await;
    let session = fixture.session(viewer).await;
    let source = uuid::Uuid::new_v4().to_string();
    let first = session.begin_share(
        &source,
        selected(Destination::Channel("elsewhere".into()), Vec::new()),
    );
    // A second view resumes the saved intent rather than replacing it.
    let second = session.begin_share(&source, selected(Destination::New, Vec::new()));
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(
        second.intent.destination,
        Destination::Channel("elsewhere".into())
    );
    // Too many people fail before anything is sent, and the attempt stays
    // for a retry or Start over.
    let crowd = (0..101).map(|n| format!("{n:064x}")).collect();
    let crowded = session.begin_share(
        "crowded",
        selected(Destination::Channel("elsewhere".into()), crowd),
    );
    let error = session
        .run_share("crowded", &crowded, &ShareOptions::default(), |_| {})
        .await
        .unwrap_err();
    assert_eq!(error, "Choose at most 100 people in each group.");
    assert!(!crowded.running());
    assert!(session.share_attempt("crowded").is_some());
    session.finish_share("crowded");
    assert!(session.share_attempt("crowded").is_none());
}
