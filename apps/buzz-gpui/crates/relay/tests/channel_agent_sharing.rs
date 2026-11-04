//! Native settings operations against an isolated relay with the same grant
//! protocol as deployment. Uses the local sharing fixture's synthetic owners.

use buzz_identity::{Identity, Purpose};
use buzz_relay_client::lifecycle::{Action, NewChannel};
use buzz_relay_client::{RelayHttp, RelayOrigin, RosterStatus, Session, SessionStatus, Store};
use nostr::nips::nip19::ToBech32;
use nostr::prelude::{Event, Keys};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn identity(byte: &str) -> Arc<Identity> {
    let keys = Keys::parse(&byte.repeat(32)).unwrap();
    Arc::new(Identity::from_nsec(&keys.secret_key().to_bech32().unwrap()).unwrap())
}

async fn ready(origin: RelayOrigin, identity: Arc<Identity>) -> Arc<Session> {
    let session = Arc::new(Session::open(
        &tokio::runtime::Handle::current(),
        origin,
        identity,
        Store::in_memory().unwrap(),
    ));
    let mut snapshot = session.snapshot();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match &snapshot.borrow().status {
                SessionStatus::Ready
                    if snapshot.borrow().live
                        && matches!(snapshot.borrow().roster, RosterStatus::Ready) =>
                {
                    return;
                }
                SessionStatus::Failed(error) => panic!("Native session failed: {error}"),
                SessionStatus::Connecting | SessionStatus::Ready => {}
            }
            snapshot.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    session
}

async fn publish(http: &RelayHttp, event: Event) {
    let response = http
        .post("/events", serde_json::to_string(&event).unwrap())
        .await
        .unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&response).unwrap();
    assert_eq!(receipt["accepted"], true, "{receipt}");
}

#[tokio::test]
#[ignore = "requires the isolated sharing relay and its synthetic membership fixture"]
async fn channel_settings_share_revoke_reopen_and_archived_channel() {
    let relay =
        std::env::var("BUZZ_CHANNEL_SHARING_RELAY").expect("set loopback sharing relay URL");
    let url = url::Url::parse(&relay).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.scheme(), "http");
    let origin = RelayOrigin::parse(&relay).unwrap();
    let owner = identity("33");
    let agent = identity("22").pubkey_hex();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let tags = vec![
        vec!["d".into(), agent.clone()],
        vec!["runtime".into(), "voltage.remote-workspace/sandbox".into()],
    ];
    let record = owner
        .template(
            Purpose::Content,
            30177,
            now,
            tags,
            json!({"name":"Brokkr","parallelism":1,"respond_to":"owner-only"}).to_string(),
        )
        .unwrap();
    publish(
        &RelayHttp::new(origin.clone(), owner.clone()),
        owner.sign(Purpose::Content, record).unwrap(),
    )
    .await;
    let session = ready(origin.clone(), owner.clone()).await;
    let channel = session
        .create_channel(NewChannel {
            name: format!(
                "sharing-settings-{}",
                &uuid::Uuid::new_v4().to_string()[..8]
            ),
            description: "Native channel sharing settings acceptance".into(),
            private: false,
            ttl: None,
            labels: Vec::new(),
        })
        .await
        .unwrap();
    // Channel creation confirms the relay state before the sidebar scan has
    // necessarily adopted it. The native Members action needs that projection.
    session.refresh_roster();
    let mut channels = session.snapshot();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if channels
                .borrow()
                .channels
                .iter()
                .any(|row| row.id == channel && row.members.contains(&owner.pubkey_hex()))
            {
                break;
            }
            channels.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    session.add_agent_member(&channel, &agent).await.unwrap();
    let snapshot = session.channel_agent_sharing(&channel).await.unwrap();
    assert!(
        snapshot
            .agents
            .iter()
            .any(|row| row.pubkey == agent && !row.enabled && row.enable_reason.is_none())
    );
    session
        .set_channel_agent_sharing(&channel, &agent, true)
        .await
        .unwrap();
    assert!(
        session
            .channel_agent_sharing(&channel)
            .await
            .unwrap()
            .agents
            .iter()
            .find(|row| row.pubkey == agent)
            .unwrap()
            .enabled
    );
    // An immediate second action must beat the first event's second-level time.
    session
        .set_channel_agent_sharing(&channel, &agent, false)
        .await
        .unwrap();
    assert!(
        !session
            .channel_agent_sharing(&channel)
            .await
            .unwrap()
            .agents
            .iter()
            .find(|row| row.pubkey == agent)
            .unwrap()
            .enabled
    );
    let reopened = ready(origin.clone(), owner.clone()).await;
    assert!(
        !reopened
            .channel_agent_sharing(&channel)
            .await
            .unwrap()
            .agents
            .iter()
            .find(|row| row.pubkey == agent)
            .unwrap()
            .enabled
    );
    let other = identity("44");
    session
        .add_member(&channel, &other.pubkey_hex())
        .await
        .unwrap();
    let outsider = ready(origin, other).await;
    assert!(
        outsider
            .set_channel_agent_sharing(&channel, &agent, true)
            .await
            .is_err()
    );
    session
        .set_channel_agent_sharing(&channel, &agent, true)
        .await
        .unwrap();
    session
        .channel_action(Action::Archive, &channel)
        .await
        .unwrap();
    let archived = session.channel_agent_sharing(&channel).await.unwrap();
    let row = archived
        .agents
        .iter()
        .find(|row| row.pubkey == agent)
        .unwrap();
    assert!(row.enabled && row.enable_reason.is_some());
    session
        .set_channel_agent_sharing(&channel, &agent, false)
        .await
        .unwrap();
    assert!(
        !session
            .channel_agent_sharing(&channel)
            .await
            .unwrap()
            .agents
            .iter()
            .find(|row| row.pubkey == agent)
            .unwrap()
            .enabled
    );
    session
        .channel_action(Action::Unarchive, &channel)
        .await
        .unwrap();
    if let Ok(path) = std::env::var("BUZZ_CHANNEL_SHARING_UI_FIXTURE") {
        assert!(path.starts_with("/tmp/"));
        let value = json!({"relay":relay,"channel":channel,"owner":owner.pubkey_hex(),"owner_nsec":owner.nsec().as_str(),"agent":agent});
        std::fs::write(&path, value.to_string()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    println!(
        "Native sharing settings passed: owner grant, immediate revoke, reopened state, non-owner denial, archived revoke."
    );
}
