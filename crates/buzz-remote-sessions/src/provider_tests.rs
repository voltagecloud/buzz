use super::*;

#[test]
fn redact_secrets_replaces_nsec() {
    let s = "key=nsec1abc123def456 other";
    let r = redact_secrets(s);
    assert!(r.contains("[REDACTED]"));
    assert!(!r.contains("nsec1abc123def456"));
}

#[test]
fn redact_secrets_replaces_token() {
    let s = r#"{"token":"sprt_tok_xyz789"}"#;
    let r = redact_secrets(s);
    assert!(r.contains("[REDACTED]"));
    assert!(!r.contains("sprt_tok_xyz789"));
}

#[test]
fn redact_secrets_with_extras_scrubs_user_env_values() {
    // If a provider echoes back a user-supplied API key in its error
    // output, the desktop must not surface that secret unredacted via
    // `last_error`. We scrub the literal values that came from the
    // request's `agent.env_vars`.
    let secret = "sk-ant-api03-abc123def456";
    let stderr = format!("auth failed with key {secret} on host api.anthropic.com");
    let r = redact_secrets_with(&stderr, &[secret]);
    assert!(r.contains("[REDACTED]"));
    assert!(!r.contains(secret));
}

#[test]
fn redact_secrets_with_extras_skips_short_values() {
    // Don't scrub values shorter than 4 chars — too noisy.
    let r = redact_secrets_with("error code: 42", &["42"]);
    assert!(r.contains("42"));
}

/// GitHub tokens are recognised by shape, so one that never passed through
/// our environment — embedded in a remote URL an installer echoes — is
/// still scrubbed. The scan runs to the next whitespace or quote, so the
/// rest of the URL goes with it; over-redaction is the safe direction.
#[test]
fn redact_secrets_with_scrubs_github_token_prefixes() {
    for token in [
        "ghp_abcdefghij0123456789",
        "gho_abcdefghij0123456789",
        "ghu_abcdefghij0123456789",
        "ghs_abcdefghij0123456789",
        "ghr_abcdefghij0123456789",
        "github_pat_abcdefghij0123456789",
    ] {
        let r = redact_secrets_with(&format!("cloning https://{token}@github.com/o/r now"), &[]);
        assert!(!r.contains(token), "leaked {token}: {r}");
        assert!(r.contains("[REDACTED]"), "got: {r}");
        assert!(r.contains("cloning"), "scan must stop at whitespace: {r}");
        assert!(r.ends_with(" now"), "scan must stop at whitespace: {r}");
    }
}

#[test]
fn redact_secrets_with_extras_terminates_when_value_substring_of_marker() {
    // Regression: an earlier impl used `while let Some(pos) = find(value)`
    // which never terminates if the user's env value is a substring of
    // the replacement marker `[REDACTED]` — each replacement
    // reintroduces the same text. Now uses `str::replace` (single-pass).
    for value in ["REDACTED", "EDACTE", "REDA", "ACTED"] {
        let r = redact_secrets_with(&format!("leak={value}"), &[value]);
        assert!(r.contains("[REDACTED]"));
    }
}

#[test]
fn redact_secrets_with_extras_handles_overlapping_secrets() {
    // Longer entries get scrubbed first so the substring "abc12" isn't
    // matched before "abc123" is consumed.
    let s = "key1=abc123 key2=abc12";
    let r = redact_secrets_with(s, &["abc12", "abc123"]);
    assert!(!r.contains("abc123"));
    assert!(!r.contains("abc12 "));
}

#[test]
fn env_secrets_from_request_extracts_string_values() {
    let req = serde_json::json!({
        "op": "deploy",
        "agent": {
            "env_vars": {
                "ANTHROPIC_API_KEY": "sk-ant-test",
                "EMPTY": "",
                "NUMERIC": 42,
            },
        },
    });
    let secrets = env_secrets_from_request(&req);
    assert!(secrets.iter().any(|v| v == "sk-ant-test"));
    // Empty and non-string values are filtered out.
    assert_eq!(secrets.len(), 1);
}

#[test]
fn env_secrets_from_request_handles_missing_shape() {
    assert!(env_secrets_from_request(&serde_json::json!({})).is_empty());
    assert!(env_secrets_from_request(&serde_json::json!({"agent": {}})).is_empty());
    assert!(env_secrets_from_request(&serde_json::json!({"agent": {"env_vars": null}})).is_empty());
}

#[test]
fn redact_env_values_in_scrubs_map_values() {
    let mut env = std::collections::BTreeMap::new();
    env.insert("ANTHROPIC_API_KEY".to_string(), "sk-ant-real".to_string());
    env.insert("EMPTY".to_string(), String::new());
    let stderr = "auth=sk-ant-real failed; other context";
    let r = redact_env_values_in(stderr, &env);
    assert!(!r.contains("sk-ant-real"));
    assert!(r.contains("[REDACTED]"));
}

#[test]
fn env_secrets_from_request_includes_resolved_launch_maps() {
    let req = serde_json::json!({
        "agent": {
            "env_vars": {"LEGACY": "legacy-secret"},
            "launch": {
                "env": {"PERSONA": "persona-secret"},
                "policy_env": {"POLICY": "policy-secret"}
            }
        }
    });
    let secrets = env_secrets_from_request(&req);
    assert_eq!(secrets.len(), 3);
    for secret in ["legacy-secret", "persona-secret", "policy-secret"] {
        assert!(secrets.iter().any(|candidate| candidate == secret));
    }
}

#[cfg(unix)]
fn write_test_provider(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn provider_spawn_waits_for_a_transient_writer() {
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider");
    write_test_provider(&provider, "exit 0");
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&provider)
        .unwrap();
    assert_eq!(
        std::process::Command::new(&provider)
            .spawn()
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::ExecutableFileBusy,
    );
    let closer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        drop(writer);
    });

    let mut child = spawn_provider(&mut std::process::Command::new(&provider))
        .expect("launch after the inherited writer closes");
    assert!(child.wait().unwrap().success());
    closer.join().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn provider_spawn_returns_persistent_write_conflicts() {
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider");
    write_test_provider(&provider, "exit 0");
    let _writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&provider)
        .unwrap();

    let error = spawn_provider(&mut std::process::Command::new(&provider)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::ExecutableFileBusy);
}

#[test]
fn provider_spawn_preserves_other_launch_errors() {
    let directory = tempfile::tempdir().unwrap();
    let error = spawn_provider(&mut std::process::Command::new(
        directory.path().join("missing"),
    ))
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[cfg(unix)]
#[test]
fn provider_deploy_negotiates_and_deploys_the_same_staged_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider");
    let log = directory.path().join("invocations");
    let body = format!(
        r#"read request
printf '%s\n' "$0" >> '{}'
case "$request" in
  *\"op\":\"info\"*) printf '%s\n' '{{"ok":true,"name":"test","version":"1.0.0","protocol_version":1,"description":"test provider","config_schema":{{}}}}' ;;
  *\"op\":\"deploy\"*) printf '%s\n' '{{"ok":true,"agent_id":"remote-1"}}' ;;
esac"#,
        log.display()
    );
    write_test_provider(&provider, &body);

    let id = provider_deploy(
        &provider,
        None,
        &serde_json::json!({}),
        &serde_json::json!({}),
    )
    .expect("staged deploy");
    assert_eq!(id, "remote-1");
    let paths: Vec<_> = std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(paths.len(), 2);
    assert_eq!(paths[0], paths[1]);
    assert_ne!(Path::new(&paths[0]), provider);
    assert!(
        !Path::new(&paths[0]).exists(),
        "staging directory must be deleted"
    );
}

#[cfg(unix)]
fn replacement_provider() -> &'static str {
    r#"#!/bin/sh
set -eu
read request
case "$request" in
  *\"op\":\"info\"*) printf '%s\n' '{"ok":true,"name":"replacement","version":"9.9.9","protocol_version":1,"description":"replacement provider","config_schema":{}}' ;;
  *\"op\":\"deploy\"*) printf '%s\n' '{"ok":true,"agent_id":"replacement-bytes-ran"}' ;;
esac
"#
}

#[cfg(unix)]
#[test]
fn provider_deploy_uses_staged_bytes_after_same_inode_source_rewrite() {
    use std::os::unix::fs::MetadataExt;

    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider");
    let replacement = directory.path().join("replacement");
    std::fs::write(&replacement, replacement_provider()).unwrap();
    let body = format!(
        r#"read request
case "$request" in
  *\"op\":\"info\"*)
    cat '{}' > '{}'
    chmod 700 '{}'
    printf '%s\n' '{{"ok":true,"name":"original","version":"1.0.0","protocol_version":1,"description":"original provider","config_schema":{{}}}}'
    ;;
  *\"op\":\"deploy\"*) printf '%s\n' '{{"ok":true,"agent_id":"original-staged-bytes"}}' ;;
esac"#,
        replacement.display(),
        provider.display(),
        provider.display(),
    );
    write_test_provider(&provider, &body);
    let inode_before = std::fs::metadata(&provider).unwrap().ino();

    let id = provider_deploy(
        &provider,
        None,
        &serde_json::json!({}),
        &serde_json::json!({}),
    )
    .expect("deploy from immutable staged copy");

    assert_eq!(id, "original-staged-bytes");
    assert_eq!(
        std::fs::metadata(&provider).unwrap().ino(),
        inode_before,
        "test must rewrite the source binary in place"
    );
    assert_eq!(
        std::fs::read_to_string(&provider).unwrap(),
        replacement_provider(),
        "source pathname must contain replacement bytes before deploy"
    );
}

#[cfg(unix)]
#[test]
fn provider_deploy_uses_staged_bytes_after_source_pathname_replacement() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider");
    let replacement = directory.path().join("replacement");
    std::fs::write(&replacement, replacement_provider()).unwrap();
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700)).unwrap();
    let body = format!(
        r#"read request
case "$request" in
  *\"op\":\"info\"*)
    mv '{}' '{}'
    printf '%s\n' '{{"ok":true,"name":"original","version":"1.0.0","protocol_version":1,"description":"original provider","config_schema":{{}}}}'
    ;;
  *\"op\":\"deploy\"*) printf '%s\n' '{{"ok":true,"agent_id":"original-staged-bytes"}}' ;;
esac"#,
        replacement.display(),
        provider.display(),
    );
    write_test_provider(&provider, &body);
    let inode_before = std::fs::metadata(&provider).unwrap().ino();

    let id = provider_deploy(
        &provider,
        None,
        &serde_json::json!({}),
        &serde_json::json!({}),
    )
    .expect("deploy from immutable staged copy");

    assert_eq!(id, "original-staged-bytes");
    assert_ne!(
        std::fs::metadata(&provider).unwrap().ino(),
        inode_before,
        "test must replace the source pathname with a different inode"
    );
    assert_eq!(
        std::fs::read_to_string(&provider).unwrap(),
        replacement_provider(),
        "source pathname must contain replacement bytes before deploy"
    );
}

#[cfg(unix)]
#[test]
fn provider_deploy_refuses_mismatch_before_sending_agent_secret() {
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider");
    let marker = directory.path().join("deploy-received");
    let body = format!(
        r#"read request
case "$request" in
  *\"op\":\"info\"*) printf '%s\n' '{{"ok":true,"name":"test","version":"2.0.0","protocol_version":2,"description":"test provider","config_schema":{{}}}}' ;;
  *\"op\":\"deploy\"*) touch '{}'; printf '%s\n' '{{"ok":true,"agent_id":"bad"}}' ;;
esac"#,
        marker.display()
    );
    write_test_provider(&provider, &body);

    let error = provider_deploy(
        &provider,
        None,
        &serde_json::json!({"private_key_nsec": "nsec1must-not-cross"}),
        &serde_json::json!({}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("protocol version 2"), "{error}");
    assert!(!marker.exists());
    assert!(!error.to_string().contains("nsec1must-not-cross"));
}

#[cfg(unix)]
#[test]
fn provider_deploy_requires_an_explicit_integer_protocol_version() {
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider");
    write_test_provider(
        &provider,
        r#"read request
printf '%s\n' '{"ok":true,"version":"1.0.0"}'"#,
    );

    let error = provider_deploy(
        &provider,
        None,
        &serde_json::json!({}),
        &serde_json::json!({}),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("missing integer protocol_version"),
        "{error}"
    );
}

#[test]
fn provider_info_requires_the_complete_flat_wire_shape() {
    let complete = serde_json::json!({
        "ok": true,
        "name": "kubernetes",
        "version": "1.0.0",
        "protocol_version": 1,
        "description": "Kubernetes provider",
        "config_schema": {}
    });
    assert!(parse_provider_info(&complete).is_ok());

    let mut missing = complete.clone();
    missing.as_object_mut().unwrap().remove("config_schema");
    assert!(parse_provider_info(&missing)
        .unwrap_err()
        .contains("config_schema"));

    let mut nested = complete;
    nested.as_object_mut().unwrap().insert(
        "provider".into(),
        serde_json::json!({"protocol_version": 1}),
    );
    assert!(parse_provider_info(&nested)
        .unwrap_err()
        .contains("unknown field provider"));
}

fn info_with_ops(ops: Option<serde_json::Value>) -> serde_json::Value {
    let mut info = serde_json::json!({
        "ok": true,
        "name": "kubernetes",
        "version": "1.0.0",
        "protocol_version": 1,
        "description": "Kubernetes provider",
        "config_schema": {}
    });
    if let Some(ops) = ops {
        info["ops"] = ops;
    }
    info
}

#[test]
fn provider_info_ops_are_parsed_and_default_to_deploy_only() {
    let advertised = info_with_ops(Some(serde_json::json!(["info", "deploy", "stop"])));
    assert!(parse_provider_info(&advertised).is_ok());
    assert_eq!(
        advertised_ops(&advertised).unwrap(),
        ["info", "deploy", "stop"]
    );

    // Providers predating `ops` keep working and have no stop.
    let legacy = info_with_ops(None);
    assert!(parse_provider_info(&legacy).is_ok());
    assert_eq!(advertised_ops(&legacy).unwrap(), ["info", "deploy"]);

    for malformed in [
        serde_json::json!("stop"),
        serde_json::json!({"stop": true}),
        serde_json::json!(["info", 1]),
        serde_json::json!(["info", ""]),
        serde_json::json!(vec!["stop"; 33]),
    ] {
        let info = info_with_ops(Some(malformed.clone()));
        let error = parse_provider_info(&info).unwrap_err();
        assert!(error.to_string().contains("ops"), "{malformed}: {error}");
    }
}

#[test]
fn stop_response_state_accepts_only_the_wire_states() {
    for (wire, state) in [
        ("ending", StopState::Ending),
        ("ended", StopState::Ended),
        ("absent", StopState::Absent),
    ] {
        let response = serde_json::json!({"ok": true, "agent_id": "a", "state": wire});
        assert_eq!(stop_response_state(&response).unwrap(), state);
        assert_eq!(serde_json::to_value(state).unwrap(), wire);
    }
    let unknown = serde_json::json!({"ok": true, "agent_id": "a", "state": "running"});
    assert!(stop_response_state(&unknown)
        .unwrap_err()
        .contains("unknown state"));
    let missing = serde_json::json!({"ok": true, "agent_id": "a"});
    assert!(stop_response_state(&missing)
        .unwrap_err()
        .contains("missing state"));
}

#[cfg(unix)]
fn stop_test_provider(path: &Path, ops: &str, marker: &Path) {
    let body = format!(
        r#"read request
case "$request" in
  *\"op\":\"info\"*) printf '%s\n' '{{"ok":true,"name":"test","version":"1.0.0","protocol_version":1,{ops}"description":"test provider","config_schema":{{}}}}' ;;
  *\"op\":\"stop\"*) touch '{}'; printf '%s\n' '{{"ok":true,"agent_id":"remote-1","state":"ending"}}' ;;
esac"#,
        marker.display()
    );
    write_test_provider(path, &body);
}

#[cfg(unix)]
#[test]
fn provider_stop_sends_stop_only_to_a_provider_that_advertises_it() {
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider");
    let marker = directory.path().join("stop-received");
    let agent = serde_json::json!({"private_key_nsec": "nsec1must-not-cross"});

    stop_test_provider(&provider, "", &marker);
    let error = provider_stop(&provider, None, &agent, &serde_json::json!({})).unwrap_err();
    assert!(
        error.to_string().contains("cannot end remote sessions"),
        "{error}"
    );
    assert!(!marker.exists(), "the secret-bearing stop must not be sent");

    stop_test_provider(&provider, r#""ops":["info","deploy"],"#, &marker);
    assert!(provider_stop(&provider, None, &agent, &serde_json::json!({})).is_err());
    assert!(!marker.exists(), "the secret-bearing stop must not be sent");

    stop_test_provider(&provider, r#""ops":["info","deploy","stop"],"#, &marker);
    assert_eq!(
        provider_info(&provider, None).unwrap().ops,
        ["info", "deploy", "stop"]
    );
    assert!(!marker.exists(), "provider_info runs only info");
    assert_eq!(
        provider_stop(&provider, None, &agent, &serde_json::json!({})).unwrap(),
        StopState::Ending
    );
    assert!(marker.exists());
}

#[cfg(unix)]
#[test]
fn provider_stop_errors_are_redacted_like_deploy() {
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("provider");
    write_test_provider(
        &provider,
        r#"read request
case "$request" in
  *\"op\":\"info\"*) printf '%s\n' '{"ok":true,"name":"test","version":"1.0.0","protocol_version":1,"ops":["info","deploy","stop"],"description":"test provider","config_schema":{}}' ;;
  *\"op\":\"stop\"*) printf '%s\n' '{"ok":false,"error":"refused env-secret-value and nsec1leak"}' ;;
esac"#,
    );
    let agent = serde_json::json!({"env_vars": {"API": "env-secret-value"}});
    let error = provider_stop(&provider, None, &agent, &serde_json::json!({})).unwrap_err();
    assert!(!error.to_string().contains("env-secret-value"), "{error}");
    assert!(!error.to_string().contains("nsec1leak"), "{error}");
    assert!(error.to_string().contains("[REDACTED]"), "{error}");
}

#[test]
fn subagent_capability_is_public_but_credentials_still_fail() {
    for enabled in [true, false] {
        assert!(
            validate_provider_config(&serde_json::json!({"pod_options": {
                "enable_subagents": enabled, "priority_class_name": "agent-session"
            }}))
            .is_ok()
        );
    }
    for credential in [
        serde_json::json!({"access_token":"secret"}),
        serde_json::json!({"api_key":"secret"}),
    ] {
        assert!(validate_provider_config(&serde_json::json!({"pod_options":credential})).is_err());
    }
}

#[test]
fn validate_provider_config_rejects_secret_key() {
    let cfg = serde_json::json!({"api_key": "val"});
    assert!(validate_provider_config(&cfg).is_err());
}

#[test]
fn validate_provider_config_bounds_nested_public_options() {
    let cfg = serde_json::json!({"pod_options": {
        "node_selector": {"workload": "agents"},
        "tolerations": [{"key":"workload", "value":"agents", "effect":"NoSchedule"}]
    }});
    assert!(validate_provider_config(&cfg).is_ok());
    let secret = serde_json::json!({"pod_options":{"nested":[{"accessToken":"secret"}]}});
    assert!(validate_provider_config(&secret).is_err());
    assert!(validate_provider_config(&serde_json::json!({"options":{"apiKey":"secret"}})).is_err());
    let too_deep = serde_json::json!({"a":{"b":{"c":{"d":{"e":1}}}}});
    assert!(validate_provider_config(&too_deep).is_err());
}

#[test]
fn validate_provider_config_allows_literal_key_only_in_tolerations() {
    for misplaced in [
        serde_json::json!({"s3": {"key": "AKIA"}}),
        serde_json::json!({"pod_options": {"key": "AKIA"}}),
        serde_json::json!({"pod_options": {"node_selector": {"key": "AKIA"}}}),
        serde_json::json!({"pod_options": {"tolerations": {"key": "AKIA"}}}),
        serde_json::json!({"options": {"tolerations": [{"key": "AKIA"}]}}),
        serde_json::json!({"pod_options": {"tolerations": [[{"key": "AKIA"}]]}}),
        serde_json::json!({"pod_options": {"tolerations": [{"nested": {"key": "AKIA"}}]}}),
    ] {
        assert!(
            validate_provider_config(&misplaced).is_err(),
            "accepted a literal key outside pod_options.tolerations[]: {misplaced}"
        );
    }
    let toleration = serde_json::json!({"pod_options": {"tolerations": [
        {"key": "workload", "operator": "Equal", "value": "agents"}
    ]}});
    assert!(validate_provider_config(&toleration).is_ok());
    let compound = serde_json::json!({"pod_options": {"tolerations": [{"apiKey": "x"}]}});
    assert!(validate_provider_config(&compound).is_err());
}

#[test]
fn validate_provider_config_accepts_scalars() {
    let cfg = serde_json::json!({"region": "us-east-1", "tier": "standard"});
    assert!(validate_provider_config(&cfg).is_ok());
}

#[test]
fn validate_provider_config_allows_key_as_substring() {
    // "keyboard", "monkey" contain "key" as substring but not as a word segment.
    let cfg = serde_json::json!({"keyboard_layout": "us", "monkey_wrench": "tight"});
    assert!(validate_provider_config(&cfg).is_ok());
}

#[test]
fn validate_provider_config_rejects_camel_case_secrets() {
    assert!(validate_provider_config(&serde_json::json!({"apiKey": "val"})).is_err());
    assert!(validate_provider_config(&serde_json::json!({"accessToken": "val"})).is_err());
    assert!(validate_provider_config(&serde_json::json!({"clientSecret": "val"})).is_err());
    // ALL-CAPS variants
    assert!(validate_provider_config(&serde_json::json!({"apiKEY": "val"})).is_err());
    assert!(validate_provider_config(&serde_json::json!({"accessTOKEN": "val"})).is_err());
}

#[test]
fn split_config_key_handles_all_styles() {
    assert_eq!(split_config_key("apiKey"), vec!["api", "key"]);
    assert_eq!(split_config_key("access_token"), vec!["access", "token"]);
    assert_eq!(split_config_key("keyboard"), vec!["keyboard"]);
    assert_eq!(split_config_key("client-secret"), vec!["client", "secret"]);
    // Acronym runs stay together
    assert_eq!(split_config_key("APIKey"), vec!["api", "key"]);
    assert_eq!(split_config_key("apiKEY"), vec!["api", "key"]);
    assert_eq!(split_config_key("accessTOKEN"), vec!["access", "token"]);
    assert_eq!(split_config_key("MyAPIKey"), vec!["my", "api", "key"]);
}

#[test]
fn provider_filename_strips_the_windows_extension() {
    assert_eq!(
        provider_id_from_filename("buzz-backend-kubernetes"),
        Some("kubernetes")
    );
    assert_eq!(
        provider_id_from_filename("buzz-backend-kubernetes.exe"),
        Some("kubernetes")
    );
    assert_eq!(
        provider_id_from_filename("buzz-backend-kubernetes.EXE"),
        Some("kubernetes")
    );
    assert_eq!(
        provider_id_from_filename("buzz-backend-kubernetes.bat"),
        Some("kubernetes")
    );
    assert_eq!(
        provider_id_from_filename("buzz-backend-kubernetes.CMD"),
        Some("kubernetes")
    );
    assert_eq!(
        provider_id_from_filename("buzz-backend-my-provider"),
        Some("my-provider")
    );
    assert_eq!(provider_id_from_filename("other"), None);
}

#[test]
fn resolve_provider_binary_rejects_invalid_ids() {
    let discovery = Discovery::default();
    for invalid in [
        "../evil",      // path traversal
        "",             // empty
        "MyProvider",   // uppercase
        "my provider",  // spaces
        "foo;rm -rf /", // shell metacharacters
        "-leading-dash",
    ] {
        let error = resolve_provider_binary(invalid, None, &discovery).unwrap_err();
        assert!(
            error.message.contains("invalid provider ID"),
            "{invalid:?}: {error}"
        );
    }
    // Valid format but not discovered.
    let error = resolve_provider_binary("nonexistent-test-id-12345", None, &discovery).unwrap_err();
    assert!(error.message.contains("not found"), "{error}");
}

#[cfg(unix)]
fn install_backend(dir: &Path, id: &str) -> PathBuf {
    let path = dir.join(format!("buzz-backend-{id}"));
    write_test_provider(&path, "exit 0");
    path
}

#[cfg(unix)]
#[test]
fn discovery_lists_executable_providers_first_directory_first() {
    use std::os::unix::fs::PermissionsExt;
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let preferred = install_backend(first.path(), "kubernetes");
    install_backend(second.path(), "kubernetes");
    install_backend(second.path(), "other");
    let not_executable = second.path().join("buzz-backend-plain");
    std::fs::write(&not_executable, "").unwrap();
    std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(second.path().join("unrelated"), "").unwrap();
    let discovery = Discovery {
        dirs: vec![first.path().to_path_buf(), second.path().to_path_buf()],
    };

    let ids: Vec<String> = discovery
        .candidates()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids.iter().filter(|id| *id == "other").count(), 1);
    assert!(!ids.iter().any(|id| id == "plain"), "{ids:?}");
    assert_eq!(
        resolve_provider_binary("kubernetes", None, &discovery).unwrap(),
        preferred.canonicalize().unwrap(),
        "the first directory wins"
    );
}

#[cfg(unix)]
#[test]
fn a_cached_binary_is_used_only_when_discovered_for_the_same_provider() {
    let bundled = tempfile::tempdir().unwrap();
    let path_dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let discovered = install_backend(bundled.path(), "kubernetes");
    let alternate = install_backend(path_dir.path(), "kubernetes");
    let other_provider = install_backend(path_dir.path(), "other");
    let undiscovered = install_backend(outside.path(), "kubernetes");
    let discovery = Discovery {
        dirs: vec![bundled.path().to_path_buf(), path_dir.path().to_path_buf()],
    };
    let resolve =
        |cached: &Path| resolve_provider_binary("kubernetes", Some(cached), &discovery).unwrap();

    // A cached path that is still the discovered candidate for this id is kept.
    assert_eq!(resolve(&discovered), discovered.canonicalize().unwrap());
    // A tampered record cannot redirect calls to another provider's binary,
    // an undiscovered or shadowed binary, or a missing file: discovery
    // decides instead.
    for tampered in [
        other_provider,
        undiscovered,
        alternate,
        outside.path().join("missing"),
    ] {
        assert_eq!(
            resolve(&tampered),
            discovered.canonicalize().unwrap(),
            "{}",
            tampered.display()
        );
    }
}

#[test]
fn discovery_searches_the_bundled_directory_first_then_path_then_local_bin() {
    let bundled = PathBuf::from("/nonexistent/bundle/Contents/MacOS");
    let discovery = Discovery::from_env(Some(bundled.clone()));
    assert_eq!(discovery.dirs.first(), Some(&bundled));
    if let Some(home) = dirs::home_dir() {
        assert!(discovery.dirs.contains(&home.join(".local").join("bin")));
    }
    let path_dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    if let Some(first_path_dir) = path_dirs.first() {
        // Already-listed directories keep their PATH position.
        let discovery = Discovery::from_env(Some(first_path_dir.clone()));
        assert_eq!(
            discovery
                .dirs
                .iter()
                .filter(|dir| *dir == first_path_dir)
                .count(),
            1
        );
    }
    assert!(!Discovery::from_env(None).dirs.contains(&bundled));
}
