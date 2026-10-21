//! What the composer publishes, against the local test relay: mentions of
//! people outside the channel and custom emoji. Skipped unless
//! `BUZZ_TEST_RELAY` points at `.test-relay`. Messages go to a throwaway
//! channel whose name carries the process id.

use buzz_identity::Identity;
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

/// The accepted echo of `content` in the channel `view` follows.
async fn echo(view: &ChannelView, content: &str) -> Arc<fold::MessageRow> {
    let mut timeline = view.snapshot.clone();
    let seen = until(&mut timeline, content, |t| {
        t.rows.iter().any(|r| {
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
async fn mentions_outsiders_as_references_or_invites_them() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = fixture.session("alice").await;
    let carol = fixture.pubkey("carol");
    let id = alice
        .create_channel(NewChannel {
            name: format!("#outsiders-{}", std::process::id()),
            description: "Created by a test".into(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    let view = alice.open_channel(&id);

    // "Do nothing": the outsider is named but not notified or added.
    let skipped = format!("@Carol not added {}", std::process::id());
    alice
        .send(
            &id,
            &skipped,
            Mentions {
                recipients: Vec::new(),
                references: vec![carol.clone()],
                ..Default::default()
            },
            Vec::new(),
            None,
        )
        .await
        .unwrap();
    let row = echo(&view, &skipped).await;
    assert!(row.mentions.is_empty());
    assert_eq!(
        row.mention_references
            .iter()
            .map(|k| k.to_hex())
            .collect::<Vec<_>>(),
        std::slice::from_ref(&carol)
    );

    // "Invite": add them, then they are a recipient like any member.
    alice.add_member(&id, &carol).await.unwrap();
    let invited = format!("@Carol welcome {}", std::process::id());
    alice
        .send(
            &id,
            &invited,
            Mentions {
                recipients: vec![carol.clone()],
                references: Vec::new(),
                ..Default::default()
            },
            Vec::new(),
            None,
        )
        .await
        .unwrap();
    let row = echo(&view, &invited).await;
    assert_eq!(
        row.mentions.iter().map(|k| k.to_hex()).collect::<Vec<_>>(),
        [carol]
    );
}

/// A real PNG: the relay decodes uploads and refuses anything else.
const PNG: &[u8] = include_bytes!("../../../assets/icons/buzz-128.png");

#[tokio::test(flavor = "multi_thread")]
async fn adds_custom_emoji_to_the_viewer_set() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let bob = fixture.session("bob").await;
    let viewer = bob.viewer().to_owned();
    let identity = {
        let nsec = fixture.identities["identities"]["bob"]["nsec"]
            .as_str()
            .unwrap();
        Arc::new(Identity::from_nsec(nsec).unwrap())
    };
    let http = RelayHttp::new(fixture.origin.clone(), identity.clone());
    let filter = emoji::own_set_filter(&viewer);
    let before = http.query(std::slice::from_ref(&filter)).await.unwrap();
    let before = before.iter().max_by_key(|event| event.created_at).cloned();

    let image = upload::upload(
        bob.media(),
        UploadFile {
            name: "wave.png".into(),
            mime: "image/png".into(),
            bytes: PNG.to_vec(),
        },
    )
    .await
    .unwrap();
    let code = format!("test-wave-{}", std::process::id());
    bob.add_emoji(&code, &image.url).await.unwrap();
    let mut snapshot = bob.snapshot();
    until(&mut snapshot, "the new emoji", |s| {
        s.emoji.mine.iter().any(|e| e.shortcode == code)
    })
    .await;
    // Replacing keeps one entry per code, now last.
    bob.add_emoji(&format!(":{code}:"), &image.url)
        .await
        .unwrap();
    let after = http.query(std::slice::from_ref(&filter)).await.unwrap();
    let after = after.iter().max_by_key(|event| event.created_at).unwrap();
    let entries = emoji::set_entries(after);
    assert_eq!(entries.iter().filter(|e| e.shortcode == code).count(), 1);
    assert_eq!(entries.last().unwrap().shortcode, code);
    if let Some(before) = &before {
        for kept in emoji::set_entries(before) {
            assert!(entries.contains(&kept), "kept {}", kept.shortcode);
        }
    }

    // Put bob's set back as it was.
    let tags = match &before {
        Some(event) => event.tags.iter().map(|t| t.as_slice().to_vec()).collect(),
        None => vec![vec!["d".to_owned(), emoji::EMOJI_SET.to_owned()]],
    };
    let unsigned = identity
        .template(
            buzz_identity::Purpose::Content,
            kinds::CUSTOM_EMOJI,
            after.created_at.as_secs() + 1,
            tags,
            String::new(),
        )
        .unwrap();
    let restored = identity
        .sign(buzz_identity::Purpose::Content, unsigned)
        .unwrap();
    http.post("/events", serde_json::to_string(&restored).unwrap())
        .await
        .unwrap();
}

/// The test relays do not advertise `buzz-gif`, and the plain one is not
/// HTTPS, so GIF search stays off without a request to the provider.
#[tokio::test(flavor = "multi_thread")]
async fn offers_gifs_only_when_the_relay_advertises_them() {
    let Some(fixture) = fixture() else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let alice = fixture.session("alice").await;
    assert!(!alice.gif_search_available().await);
    assert_eq!(
        alice.search_gifs("cats", "test", "en-US").await,
        Err(gifs::UNAVAILABLE.to_owned())
    );
}
