//! The live agent activity feed against the local TLS test relay, which
//! admits agents through their owner's NIP-OA attestation. Skipped unless
//! `BUZZ_TEST_RELAY_TLS` points at `.test-relay` (after
//! `scripts/test-relay.sh up-tls`).
//!
//! A throwaway agent key, attested by alice over NIP-OA, publishes one
//! encrypted kind 24200 frame to her. Her session's activity feed must
//! report listening, deliver the envelope, and her key must decrypt it.
//! The relay records the throwaway agent's owner link; nothing else is
//! written.

use buzz_identity::Identity;
use buzz_relay_client::session::{ActivityFeed, FeedStatus};
use buzz_relay_client::*;
use futures::{SinkExt, StreamExt};
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp, nip44};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as Ws;

/// NIP-OA: the owner's signature over `nostr:agent-auth:<agent>:`.
fn auth_tag(owner: &Keys, agent: &Keys) -> Tag {
    let preimage = format!("nostr:agent-auth:{}:", agent.public_key().to_hex());
    let digest: [u8; 32] = Sha256::digest(preimage.as_bytes()).into();
    let signature = owner.sign_schnorr(digest);
    Tag::parse([
        "auth",
        &owner.public_key().to_hex(),
        "",
        &signature.to_string(),
    ])
    .unwrap()
}

/// Connects as the agent, authenticates with alice's attestation and
/// publishes `frame`, returning the relay's OK.
async fn publish_as_agent(url: &str, agent: &Keys, owner: &Keys, frame: &Event) -> Value {
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let mut authed = false;
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("the relay answers")
            .unwrap()
            .unwrap();
        let Ws::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        match value[0].as_str() {
            Some("AUTH") => {
                let challenge = value[1].as_str().unwrap();
                let auth = EventBuilder::new(Kind::Authentication, "")
                    .tags([
                        Tag::parse(["relay", url]).unwrap(),
                        Tag::parse(["challenge", challenge]).unwrap(),
                        auth_tag(owner, agent),
                    ])
                    .finalize(agent)
                    .unwrap();
                socket
                    .send(Ws::text(json!(["AUTH", auth]).to_string()))
                    .await
                    .unwrap();
            }
            Some("OK") if !authed => {
                assert_eq!(value[2], true, "agent AUTH: {value}");
                authed = true;
                socket
                    .send(Ws::text(json!(["EVENT", frame]).to_string()))
                    .await
                    .unwrap();
            }
            Some("OK") => return value,
            _ => {}
        }
    }
}

async fn next(feed: &mut tokio::sync::mpsc::UnboundedReceiver<ActivityFeed>) -> ActivityFeed {
    tokio::time::timeout(Duration::from_secs(20), feed.recv())
        .await
        .expect("the feed moves")
        .expect("the feed stays open")
}

#[tokio::test(flavor = "multi_thread")]
async fn alice_receives_and_decodes_her_agents_activity() {
    let Ok(dir) = std::env::var("BUZZ_TEST_RELAY_TLS") else {
        eprintln!("BUZZ_TEST_RELAY_TLS not set; skipping");
        return;
    };
    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/{name}")).unwrap()).unwrap()
    };
    let identities = read("identities.json");
    let tls = read("tls/relay.json");
    // SAFETY: set before this test starts any thread that reads it.
    unsafe { std::env::set_var("SSL_CERT_FILE", tls["ca_bundle"].as_str().unwrap()) };
    let url = tls["relay_ws_url"].as_str().unwrap().to_string();
    let origin = RelayOrigin::parse(&url).unwrap();
    let alice_entry = &identities["identities"]["alice"];
    let alice = Arc::new(Identity::from_nsec(alice_entry["nsec"].as_str().unwrap()).unwrap());
    let owner = Keys::parse(alice_entry["sk_hex"].as_str().unwrap()).unwrap();

    let session = Session::open(
        &tokio::runtime::Handle::current(),
        origin,
        alice.clone(),
        Store::in_memory().unwrap(),
    );
    let mut feed = session.observe_activity();
    loop {
        if let ActivityFeed::Status(FeedStatus::Listening) = next(&mut feed).await {
            break;
        }
    }

    let agent = Keys::generate();
    let plaintext = json!({
        "kind": "turn_started",
        "turnId": "integration-turn",
        "channelId": null,
        "timestamp": "2026-10-10T12:00:00Z",
        "payload": {"source": "test"},
    })
    .to_string();
    let content = nip44::encrypt(
        agent.secret_key(),
        &alice.public_key(),
        &plaintext,
        nip44::Version::V2,
    )
    .unwrap();
    let frame = EventBuilder::new(Kind::from_u16(24200), content)
        .tags([
            Tag::parse(["p", &alice.pubkey_hex()]).unwrap(),
            Tag::parse(["agent", &agent.public_key().to_hex()]).unwrap(),
            Tag::parse(["frame", "telemetry"]).unwrap(),
        ])
        .custom_created_at(Timestamp::now())
        .finalize(&agent)
        .unwrap();
    let ok = publish_as_agent(&url, &agent, &owner, &frame).await;
    assert_eq!(ok[2], true, "the relay took the frame: {ok}");

    let received = loop {
        match next(&mut feed).await {
            ActivityFeed::Frame(event) if event.id == frame.id => break event,
            _ => {}
        }
    };
    // The viewer's key decrypts it through buzz-desktop's decoder.
    let wire = serde_json::from_value(serde_json::to_value(&*received).unwrap()).unwrap();
    let decoded = alice.decode_agent_frame(&wire, false).unwrap();
    assert_eq!(decoded["agent"], agent.public_key().to_hex());
    assert_eq!(decoded["plaintext"], plaintext);
    // Nobody else can read it.
    let someone = Identity::ephemeral();
    assert!(someone.decode_agent_frame(&wire, false).is_err());
}
