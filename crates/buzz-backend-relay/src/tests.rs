//! Provider behavior that needs no relay. The exchange with the session
//! manager is tested end to end in `buzz-sandbox-reaper`
//! (`serve::relay_provider_tests`), against its fake relay and the manager.

use super::*;
use nostr::nips::nip19::ToBech32;
use serde_json::json;

/// A loopback port with nothing listening.
fn closed_relay() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    drop(listener);
    url
}

/// The request the desktop sends once the thread is bound.
fn request(op: &str, relay_url: &str) -> Value {
    let (agent, owner) = (Keys::generate(), Keys::generate());
    json!({
        "op": op,
        "request_id": "r-1",
        "agent": {"relay_url": "wss://community.example.invalid",
            "private_key_nsec": agent.secret_key().to_bech32().unwrap(),
            "auth_tag": buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap(),
            "launch": {"owner_pubkey": owner.public_key().to_hex()}},
        "provider_config": {"relay_url": relay_url, "manager_pubkey": "a".repeat(64),
            "sandbox": {"channel_id": "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10",
                "thread_root": "b".repeat(64)},
            "identity_policy": {"agent_pubkey": agent.public_key().to_hex(),
                "owner_pubkey": owner.public_key().to_hex()}}
    })
}

#[test]
fn info_answers_locally_and_advertises_the_session_lifecycle() {
    let info = respond(r#"{"op":"info"}"#);
    assert_eq!(info["ok"], true);
    assert_eq!(info["name"], "relay");
    assert_eq!(info["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(info["ops"], json!(["info", "deploy", "stop", "recover"]));
    assert_eq!(
        info["config_schema"]["required"],
        json!(["relay_url", "manager_pubkey"])
    );
}

/// Every local refusal answers before the relay is contacted: the relay
/// here does not exist, so contacting it would answer `relay_unavailable`.
#[tokio::test]
async fn local_refusals_precede_relay_contact() {
    let relay = closed_relay();
    let without = |op: &str, pointer: &str| {
        let mut value = request(op, &relay);
        let (parent, field) = pointer.rsplit_once('/').unwrap();
        value
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(field);
        value
    };
    let with = |op: &str, pointer: &str, bad: Value| {
        let mut value = request(op, &relay);
        let (parent, field) = pointer.rsplit_once('/').unwrap();
        value.pointer_mut(parent).unwrap()[field] = bad;
        value
    };
    let cases = [
        (
            without("stop", "/provider_config/identity_policy"),
            "identity_policy",
        ),
        (
            with("deploy", "/provider_config/sandbox", json!(false)),
            "Sandbox sessions only",
        ),
        (
            with("deploy", "/provider_config/sandbox", json!(true)),
            "channel_id",
        ),
        (
            with("deploy", "/provider_config/manager_pubkey", json!("x")),
            "manager_pubkey",
        ),
        (
            with("stop", "/provider_config/relay_url", json!("http://x")),
            "relay_url",
        ),
        (
            with("deploy", "/agent/auth_tag", json!("not-an-attestation")),
            "attestation",
        ),
        (
            with("deploy", "/agent/provider", json!("relay-mesh")),
            "relay-mesh",
        ),
        (
            with(
                "deploy",
                "/provider_config/namespace",
                json!("buzz-agent-staging-x"),
            ),
            "namespace",
        ),
    ];
    for (value, expected) in cases {
        let response = run(value, ACCEPT_WITHIN).await;
        assert_eq!(response["ok"], false, "{expected}: {response}");
        assert_ne!(response["code"], "relay_unavailable", "{expected}");
        let error = response["error"].as_str().unwrap();
        assert!(error.contains(expected), "{expected}: {error}");
    }
    assert_eq!(respond("not json")["error"], "request is not valid JSON");
}

#[tokio::test]
async fn an_unreachable_relay_is_relay_unavailable_with_no_detail() {
    let response = run(request("deploy", &closed_relay()), ACCEPT_WITHIN).await;
    assert_eq!(
        response,
        json!({"ok": false, "code": "relay_unavailable",
            "error": "could not connect, authenticate or subscribe to the relay"})
    );
}

/// The manager receives only the `provider_config` fields it reads; the
/// rest of the request is forwarded unchanged.
#[test]
fn the_forwarded_payload_keeps_only_the_managers_fields() {
    let raw = request("deploy", "wss://relay.example.com");
    let payload = forwarded(raw.clone());
    let config = payload["provider_config"].as_object().unwrap();
    let mut fields: Vec<&str> = config.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(fields, ["identity_policy", "sandbox"]);
    assert_eq!(payload["agent"], raw["agent"]);
    assert_eq!(payload["request_id"], "r-1");
}
