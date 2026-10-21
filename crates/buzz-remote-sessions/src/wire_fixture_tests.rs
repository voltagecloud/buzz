//! The client side of the provider's golden wire fixtures.
//!
//! `crates/buzz-backend-kubernetes/tests/fixtures/provider-wire` holds the
//! exact responses the provider binary produces (asserted by that crate's
//! `tests/wire_fixtures.rs`). This crate does not depend on the provider, so
//! it reads the same files and checks that the parsing it applies to every
//! response accepts each one with the fixture's own values.

use super::*;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../buzz-backend-kubernetes/tests/fixtures/provider-wire")
}

fn read_json(name: &str) -> serde_json::Value {
    let path = fixtures().join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Parse `response` to `op` exactly as the client does after a provider
/// call, reporting what it understood.
fn client_reading(op: &str, response: &serde_json::Value) -> String {
    if let Some(failure) = reported_failure(response, &[]) {
        return format!("failed {:?} {:?}", failure.code, failure.message);
    }
    match op {
        "info" => format!("{:?}", parse_provider_info(response).map(|info| info.ops)),
        "deploy" => format!("{:?}", deployed_agent_id(response)),
        "stop" => format!("{:?}", stop_response_state(response)),
        other => panic!("fixture for unknown op {other}"),
    }
}

#[test]
fn the_client_reads_every_provider_response_fixture() {
    let mut cases: Vec<String> = std::fs::read_dir(fixtures())
        .expect("could not read the provider fixture directory")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| Some(name.strip_suffix(".response.json")?.to_string()))
        .collect();
    cases.sort();
    let mut ops = std::collections::BTreeSet::new();
    for case in &cases {
        let request = read_json(&format!("{case}.request.json"));
        let response = read_json(&format!("{case}.response.json"));
        let op = request["op"].as_str().expect("request op").to_string();
        let reading = client_reading(&op, &response);
        if response["ok"] == false {
            let message = response["error"].as_str().expect("error text");
            let code = response
                .get("code")
                .map(|code| code.as_str().expect("a code is a string").to_string());
            assert_eq!(
                reading,
                format!("failed {code:?} {message:?}"),
                "{case}: the failure, its text, and its code must survive"
            );
        } else {
            assert!(reading.starts_with("Ok("), "{case}: {reading}");
        }
        ops.insert(op);
    }
    // The directory scan is the completeness guard; this keeps it from
    // passing on an empty or moved directory.
    assert!(cases.len() >= 6, "{cases:?}");
    assert!(ops.contains("deploy") && ops.contains("stop"), "{ops:?}");
}

/// The provider has no fixtures for its success responses or a coded
/// refusal: a success needs a cluster, and `info` carries the release
/// version. These are the shapes its serializer tests pin
/// (`buzz-backend-kubernetes/src/wire.rs`).
#[test]
fn the_client_reads_the_provider_serializer_shapes() {
    let info = serde_json::json!({
        "ok": true, "name": "kubernetes", "version": "0.1.0", "protocol_version": 1,
        "ops": ["info", "deploy", "stop", "recover"],
        "description": "Runs agents as pods in a Kubernetes cluster",
        "config_schema": {"type": "object"},
    });
    let parsed = parse_provider_info(&info).unwrap();
    assert!(parsed.supports_lifecycle());
    assert_eq!(parsed.name, "kubernetes");
    assert_eq!(parsed.config_schema, serde_json::json!({"type": "object"}));

    assert_eq!(
        deployed_agent_id(&serde_json::json!({"ok": true, "agent_id": "buzz-agent-abc"})).unwrap(),
        "buzz-agent-abc"
    );
    assert_eq!(
        stop_response_state(
            &serde_json::json!({"ok": true, "agent_id": "buzz-agent-abc-123", "state": "ending"})
        )
        .unwrap(),
        StopState::Ending
    );
    let refusal = serde_json::json!(
        {"ok": false, "error": "not running", "code": "session_not_running"}
    );
    assert_eq!(
        reported_failure(&refusal, &[]),
        Some(SessionError {
            message: "not running".to_string(),
            code: Some("session_not_running".to_string()),
        })
    );
    let malformed_code = serde_json::json!({"ok": false, "error": "x", "code": "Not A Code"});
    assert_eq!(reported_failure(&malformed_code, &[]).unwrap().code, None);
}
