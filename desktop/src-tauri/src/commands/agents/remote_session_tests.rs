use super::*;
use crate::commands::agents::{tests::*, workspace_owner_hex};
use crate::managed_agents::save_managed_agents;
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, path::Path};
use tauri::Manager;

fn install_provider(home: &Path, ops: Value, response: Value) {
    let path = home.join("bin/buzz-backend-kubernetes");
    let info = json!({"ok": true, "name": "kubernetes", "version": "1",
        "protocol_version": 1, "description": "Fixture provider", "config_schema": {}, "ops": ops});
    std::fs::write(&path, format!(
        "#!/bin/sh\nIFS= read -r request\ncase \"$request\" in\n*'\"op\":\"info\"'*) printf '%s\\n' '{info}' ;;\n*) printf '%s\\n' \"$request\" > '{}/request.json'\nprintf '%s\\n' '{response}' ;;\nesac\n",
        home.display(),
    )).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn lifecycle_actions_enforce_scope_and_preserve_provider_codes() {
    const CHILD: &str = "BUZZ_REMOTE_SESSION_TEST_HOME";
    let Some(home) = std::env::var_os(CHILD) else {
        return run_in_confined_child(
            "commands::agents::remote_session::tests::lifecycle_actions_enforce_scope_and_preserve_provider_codes",
            CHILD,
        );
    };
    let home = Path::new(&home);
    let app = confined_mock_app(home);
    let state = app.state::<AppState>();
    let owner = workspace_owner_hex(&state).unwrap();
    let config = json!({"sandbox": true, "namespace": "agents"});
    save_managed_agents(
        app.handle(),
        &[provider_record(
            "thread-agent",
            "kubernetes",
            config.clone(),
        )],
    )
    .unwrap();
    let session: RemoteSessionScope = serde_json::from_value(json!({
        "channelId": "f9ee507e-d03c-4570-baa6-02951c38e1cd", "threadRoot": "a".repeat(64),
    }))
    .unwrap();
    let relay = "http://127.0.0.1:9";
    let capture = home.join("request.json");
    let end = |relay: &str, signer: &str| {
        tauri::async_runtime::block_on(end_session(
            app.handle(),
            &state,
            "thread-agent",
            &session,
            relay,
            signer,
        ))
    };
    let recover = |mode, relay: &str, signer: &str| {
        tauri::async_runtime::block_on(recover_session(
            app.handle(),
            &state,
            "thread-agent",
            &session,
            mode,
            relay,
            signer,
        ))
    };
    install_provider(
        home,
        json!(["stop", "recover"]),
        json!({"ok": true, "agent_id": "remote", "state": "ended"}),
    );
    for (expected_relay, signer, message) in [
        ("", owner.as_str(), "require the community and identity"),
        (relay, " ", "require the community and identity"),
        (
            "http://other.invalid",
            owner.as_str(),
            "active community changed",
        ),
        (relay, "wrong-owner", "active identity changed"),
    ] {
        for error in [
            end(expected_relay, signer).unwrap_err(),
            recover(RemoteRecoveryMode::Fresh, expected_relay, signer).unwrap_err(),
        ] {
            assert!(error.message.contains(message), "{error}");
            assert!(error.message.contains("nothing was changed"), "{error}");
        }
        assert!(
            !capture.exists(),
            "a refused action must not invoke the provider"
        );
    }
    for stop_state in ["ending", "ended", "absent"] {
        install_provider(
            home,
            json!(["stop", "recover"]),
            json!({"ok": true, "agent_id": "remote", "state": stop_state}),
        );
        assert_eq!(end(relay, &owner).unwrap(), stop_state);
        let request: Value =
            serde_json::from_str(&std::fs::read_to_string(&capture).unwrap()).unwrap();
        assert_eq!(request["op"], "stop");
        assert_eq!(
            request["provider_config"]["identity_policy"]["owner_pubkey"],
            owner
        );
        assert_eq!(
            request["provider_config"]["sandbox"]["thread_root"],
            "a".repeat(64)
        );
        assert!(request["provider_config"]["sandbox"]
            .get("recovery")
            .is_none());
    }
    for mode in [RemoteRecoveryMode::Fresh, RemoteRecoveryMode::Checkpoint] {
        install_provider(
            home,
            json!(["stop", "recover"]),
            json!({"ok": true, "agent_id": "remote"}),
        );
        recover(mode, relay, &owner).unwrap();
        let request: Value =
            serde_json::from_str(&std::fs::read_to_string(&capture).unwrap()).unwrap();
        assert_eq!(request["op"], "deploy");
        assert_eq!(
            request["provider_config"]["sandbox"]["recovery"]["mode"],
            json!(mode)
        );
        assert_eq!(request["agent"]["launch"]["owner_pubkey"], owner);
        let records = load_managed_agents(app.handle()).unwrap();
        assert!(
            matches!(&records[0].backend, BackendKind::Provider { config: saved, .. } if saved == &config)
        );
    }
    for code in [Some("no_checkpoint"), None] {
        let mut response = json!({"ok": false, "error": "fixture refusal"});
        if let Some(code) = code {
            response["code"] = json!(code);
        }
        install_provider(home, json!(["stop", "recover"]), response);
        for error in [
            end(relay, &owner).unwrap_err(),
            recover(RemoteRecoveryMode::Checkpoint, relay, &owner).unwrap_err(),
        ] {
            assert_eq!(error.code.as_deref(), code);
            assert!(error.message.contains("fixture refusal"), "{error}");
            let serialized = serde_json::to_value(&error).unwrap();
            if let Some(code) = code {
                assert_eq!(serialized["code"], code);
            } else {
                assert_eq!(serialized, error.message);
            }
        }
    }
    for (ops, expected) in [
        (json!(["info", "deploy", "stop"]), false),
        (json!(["info", "deploy", "recover"]), false),
        (json!(["info", "deploy", "stop", "recover"]), true),
    ] {
        install_provider(home, ops, json!({"ok": true}));
        assert_eq!(
            tauri::async_runtime::block_on(supports_lifecycle(
                app.handle(),
                &state,
                "thread-agent"
            ))
            .unwrap(),
            expected
        );
    }
}
