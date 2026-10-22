//! `buzz-backend-relay` end to end: the provider library, the fake relay
//! from [`super::tests`], and this manager in process against the provider's
//! in-memory apiserver.

use super::tests::{keys, FakeRelay};
use super::*;
use buzz_backend_kubernetes::sandbox_fake;
use buzz_session_control::open_request;
use nostr::nips::nip19::ToBech32;
use nostr::PublicKey;
use serde_json::{json, Value};

/// The longest any exchange in these tests may take.
const BOUND: Duration = Duration::from_secs(20);

fn image() -> String {
    format!("example.invalid/buzz-agent@sha256:{}", "c".repeat(64))
}

/// What the deployment sets in `BUZZ_SANDBOX_PROVIDER_CONFIG`.
fn defaults() -> String {
    json!({"image": image(), "pod_options": {"active_deadline_seconds": 3600}}).to_string()
}

/// The manager for one developer (owner `33`), serving `relay` until the
/// returned sender is dropped or fired.
async fn start_manager(
    relay: &mut FakeRelay,
    manager_keys: &Keys,
) -> (
    sandbox_fake::Shared,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let config = ManagerConfig::parse(
        &json!({"version": 1, "developers": {"dev": {
            "owner_pubkey": keys("33").public_key().to_hex(),
            "namespace": "buzz-agent-staging-dev",
            "profile_secret_arn": "arn:aws:secretsmanager:us-west-2:000000000000:secret:voltage/staging/agents/dev/profile-AbCdEf"}}})
        .to_string(),
    )
    .unwrap();
    let cluster = sandbox_fake::Shared::default();
    let manager = Arc::new(
        Manager::new(
            manager_keys.clone(),
            config,
            sandbox_fake::client(&cluster),
            crate::enroll::tests::offline(Default::default()),
        )
        .with_sandbox_defaults(sandbox_defaults(Some(&defaults())).unwrap()),
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let url = relay.url.clone();
    let server = tokio::spawn(async move {
        run(manager, &url, None, async {
            let _ = stopped.await;
        })
        .await;
    });
    relay.wait_for_subscriber(&manager_keys.public_key()).await;
    (cluster, stop, server)
}

/// The request the desktop sends the relay provider for agent `22`, owned by
/// `33`, once the thread is bound.
fn provider_request(op: &str, relay: &str, manager: &PublicKey, timeout: u64) -> Value {
    let (agent, owner) = (keys("22"), keys("33"));
    json!({
        "op": op,
        "request_id": format!("{op}-1"),
        "agent": {"relay_url": "wss://community.example.invalid",
            "private_key_nsec": agent.secret_key().to_bech32().unwrap(),
            "auth_tag": buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap(),
            "launch": {"owner_pubkey": owner.public_key().to_hex(),
                "env": {"BUZZ_CHECKPOINT_BUCKET": "bucket", "BUZZ_CHECKPOINT_PREFIX": "dev/",
                    "ANTHROPIC_API_KEY": "sk-ant-fixture-do-not-leak"}}},
        "provider_config": {"relay_url": relay, "manager_pubkey": manager.to_hex(),
            "timeout_seconds": timeout,
            "sandbox": {"channel_id": "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10",
                "thread_root": "b".repeat(64)},
            "identity_policy": {"agent_pubkey": agent.public_key().to_hex(),
                "owner_pubkey": owner.public_key().to_hex()}}
    })
}

/// Deploy and stop through the provider: the manager runs them in the
/// owner's namespace with its own image, and the provider prints the
/// manager's responses. The agent's events show nothing but ciphertext.
#[tokio::test]
async fn deploy_and_stop_round_trip_through_the_manager() {
    let mut relay = FakeRelay::start().await;
    let manager_keys = keys("11");
    let manager = manager_keys.public_key();
    let (cluster, stop, server) = start_manager(&mut relay, &manager_keys).await;

    let deploy = provider_request("deploy", &relay.url, &manager, 600);
    let response = buzz_backend_relay::run(deploy, BOUND).await;
    assert_eq!(response["ok"], true, "{response}");
    let agent_id = response["agent_id"].as_str().unwrap().to_string();
    {
        let cluster = cluster.lock().unwrap();
        let sandbox = cluster.sandboxes[&agent_id].to_string();
        assert!(sandbox.contains(&image()), "the manager's image is used");
        for secret in cluster.secrets.values() {
            assert_eq!(secret["metadata"]["namespace"], "buzz-agent-staging-dev");
        }
    }

    let stop_request = provider_request("stop", &relay.url, &manager, 600);
    let response = buzz_backend_relay::run(stop_request, BOUND).await;
    assert_eq!(
        response,
        json!({"ok": true, "agent_id": agent_id, "state": "ending"})
    );

    let agent = keys("22");
    let sent = relay.published_by(&agent.public_key());
    assert_eq!(sent.len(), 2);
    let nsec = agent.secret_key().to_bech32().unwrap();
    for event in sent {
        let json = serde_json::to_string(&event).unwrap();
        for needle in [
            nsec.as_str(),
            "sk-ant-fixture",
            "deploy-1",
            "stop-1",
            "buzz-agent-staging",
            "thread_root",
        ] {
            assert!(!json.contains(needle), "{needle} visible in {json}");
        }
        let tags: Vec<&str> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice()[0].as_str())
            .collect();
        assert_eq!(tags, ["p", "auth", "expiration"]);
    }

    stop.send(()).unwrap();
    tokio::time::timeout(BOUND, server).await.unwrap().unwrap();
}

/// No manager is connected, so no one accepts the request.
#[tokio::test]
async fn no_acceptance_is_manager_unavailable() {
    let relay = FakeRelay::start().await;
    let request = provider_request("deploy", &relay.url, &keys("11").public_key(), 600);
    let response = buzz_backend_relay::run(request, Duration::from_millis(300)).await;
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], "manager_unavailable", "{response}");
    assert_eq!(relay.published_by(&keys("22").public_key()).len(), 1);
}

/// A manager that accepts but never finishes: the provider gives up at its
/// `timeout_seconds`.
#[tokio::test]
async fn acceptance_without_done_is_manager_timeout() {
    let mut relay = FakeRelay::start().await;
    let manager_keys = keys("11");
    let mut stub = NostrWsConnection::connect_authenticated(&relay.url, &manager_keys, None)
        .await
        .unwrap();
    let filter = json!({"kinds": [KIND], "#p": [manager_keys.public_key().to_hex()]});
    stub.send_raw(&json!(["REQ", "requests", filter]))
        .await
        .unwrap();
    relay.wait_for_subscriber(&manager_keys.public_key()).await;
    let accepting = tokio::spawn(async move {
        loop {
            let Ok(RelayMessage::Event { event, .. }) = stub.next_event(BOUND).await else {
                continue;
            };
            let opened = open_request(&event, &manager_keys).unwrap();
            let accepted = build_reply(
                &manager_keys,
                &opened.signer,
                &opened.event_id,
                &Reply::accepted(opened.request.request_id),
                Timestamp::now(),
            )
            .unwrap();
            stub.send_event(accepted).await.unwrap();
        }
    });

    let request = provider_request("deploy", &relay.url, &keys("11").public_key(), 1);
    let response = buzz_backend_relay::run(request, BOUND).await;
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], "manager_timeout", "{response}");
    accepting.abort();
}

/// A relay that refuses the agent's AUTH: the provider reports the failure
/// without repeating the relay's message.
#[tokio::test]
async fn a_refused_auth_is_relay_unavailable_and_redacted() {
    let relay = FakeRelay::start_with(false).await;
    let request = provider_request("stop", &relay.url, &keys("11").public_key(), 600);
    let response = buzz_backend_relay::run(request, BOUND).await;
    assert_eq!(response["code"], "relay_unavailable", "{response}");
    assert!(!response
        .to_string()
        .contains("relay-detail-not-for-clients"));
    assert!(relay.published_by(&keys("22").public_key()).is_empty());
}

#[test]
fn sandbox_provider_config_is_checked_at_startup() {
    assert!(sandbox_defaults(None).unwrap().is_empty());
    assert_eq!(
        sandbox_defaults(Some(&defaults())).unwrap()["image"],
        image()
    );
    for (text, expected) in [
        ("{", "not valid JSON"),
        ("[]", "JSON object"),
        (r#"{"namespace": "buzz-agent-staging-x"}"#, "namespace"),
        (r#"{"image": "example.invalid/x:latest"}"#, "image"),
    ] {
        let error = sandbox_defaults(Some(text)).unwrap_err().to_string();
        assert!(error.contains(expected), "{text}: {error}");
    }
}
