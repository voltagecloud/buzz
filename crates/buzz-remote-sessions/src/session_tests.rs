use super::*;

fn scope(root: &str) -> SessionScope {
    SessionScope {
        channel_id: uuid::Uuid::new_v4(),
        thread_root: root.to_string(),
    }
}

#[test]
fn sandbox_scope_is_required_and_bound_to_the_rebuilt_owner() {
    let scope = scope(&"a".repeat(64));
    let mut config = serde_json::json!({"sandbox":true});
    let payload = serde_json::json!({"launch":{"owner_pubkey":"owner"}});
    assert!(bind_sandbox_scope(&mut config, None, "agent", &payload, None).is_err());
    assert_eq!(config["sandbox"], true);
    bind_sandbox_scope(&mut config, Some(&scope), "agent", &payload, None).unwrap();
    assert_eq!(config["sandbox"]["thread_root"], scope.thread_root);
    assert_eq!(
        config["sandbox"]["channel_id"],
        scope.channel_id.to_string()
    );
    assert_eq!(config["identity_policy"]["owner_pubkey"], "owner");
    assert_eq!(config["identity_policy"]["agent_pubkey"], "agent");
    assert!(
        config["sandbox"].get("recovery").is_none(),
        "an ordinary deploy must never carry a recovery mode"
    );
    let mut missing_owner = serde_json::json!({"sandbox":true});
    let error = bind_sandbox_scope(
        &mut missing_owner,
        Some(&scope),
        "agent",
        &serde_json::json!({}),
        None,
    )
    .unwrap_err();
    assert!(error.contains("verified owner"), "{error}");
    assert_eq!(missing_owner, serde_json::json!({"sandbox": true}));
}

#[test]
fn only_a_canonical_thread_root_is_bound() {
    let payload = serde_json::json!({"launch":{"owner_pubkey":"owner"}});
    for root in [
        "A".repeat(64),
        "a".repeat(63),
        "a".repeat(65),
        format!("{}g", "a".repeat(63)),
        String::new(),
    ] {
        let mut config = serde_json::json!({"sandbox": true});
        let error = bind_sandbox_scope(&mut config, Some(&scope(&root)), "agent", &payload, None)
            .unwrap_err();
        assert!(error.contains("canonical thread root"), "{root:?}: {error}");
        assert_eq!(config, serde_json::json!({"sandbox": true}));
        assert!(SessionScope::new(uuid::Uuid::new_v4(), root).is_err());
    }
    assert!(SessionScope::new(uuid::Uuid::new_v4(), "0f".repeat(32)).is_ok());
}

#[test]
fn recovery_mode_rides_the_bound_invocation_config() {
    let payload = serde_json::json!({"launch":{"owner_pubkey":"owner"}});
    for (mode, wire) in [
        (RecoveryMode::Checkpoint, "checkpoint"),
        (RecoveryMode::Fresh, "fresh"),
    ] {
        let mut invoked = serde_json::json!({"sandbox": true});
        bind_sandbox_scope(
            &mut invoked,
            Some(&scope(&"b".repeat(64))),
            "agent",
            &payload,
            Action::Recover(mode).recovery(),
        )
        .unwrap();
        assert_eq!(
            invoked["sandbox"]["recovery"],
            serde_json::json!({"mode": wire})
        );
        assert_eq!(invoked["sandbox"]["thread_root"], "b".repeat(64));
    }
    assert_eq!(Action::Deploy.recovery(), None);
    assert_eq!(Action::End.recovery(), None);
}

#[test]
fn recovery_is_refused_without_a_thread_workspace() {
    let payload = serde_json::json!({"launch":{"owner_pubkey":"owner"}});
    let mut sandbox = serde_json::json!({"sandbox": true});
    let error = bind_sandbox_scope(
        &mut sandbox,
        None,
        "agent",
        &payload,
        Some(RecoveryMode::Fresh),
    )
    .unwrap_err();
    assert!(error.contains("channel thread"), "{error}");
    assert_eq!(sandbox, serde_json::json!({"sandbox": true}));
}

#[test]
fn recovery_mode_deserializes_from_lowercase_wire_names() {
    assert_eq!(
        serde_json::from_str::<RecoveryMode>(r#""checkpoint""#).unwrap(),
        RecoveryMode::Checkpoint
    );
    assert_eq!(
        serde_json::from_str::<RecoveryMode>(r#""fresh""#).unwrap(),
        RecoveryMode::Fresh
    );
    assert!(serde_json::from_str::<RecoveryMode>(r#""Checkpoint""#).is_err());
}

#[test]
fn session_scope_reads_the_camel_case_wire_shape_only() {
    let scope: SessionScope = serde_json::from_value(serde_json::json!({
        "channelId": "f9ee507e-d03c-4570-baa6-02951c38e1cd", "threadRoot": "a".repeat(64),
    }))
    .unwrap();
    assert_eq!(
        scope.channel_id.to_string(),
        "f9ee507e-d03c-4570-baa6-02951c38e1cd"
    );
    assert!(serde_json::from_value::<SessionScope>(serde_json::json!({
        "channelId": "f9ee507e-d03c-4570-baa6-02951c38e1cd", "threadRoot": "a".repeat(64),
        "extra": true,
    }))
    .is_err());
}

#[test]
fn thread_sandbox_is_only_the_kubernetes_provider_with_sandbox_true() {
    for (provider, config, expected) in [
        ("kubernetes", serde_json::json!({"sandbox": true}), true),
        ("kubernetes", serde_json::json!({"sandbox": false}), false),
        ("kubernetes", serde_json::json!({"sandbox": "true"}), false),
        ("kubernetes", serde_json::json!({}), false),
        ("other", serde_json::json!({"sandbox": true}), false),
    ] {
        assert_eq!(
            is_thread_sandbox(provider, &config),
            expected,
            "{provider} {config}"
        );
    }
}

// ── assert_payload_scope: post-lock rebuilt-payload validation ──────────

fn scoped_payload(relay: &str, owner: &str) -> serde_json::Value {
    serde_json::json!({
        "relay_url": relay,
        "launch": { "owner_pubkey": owner },
    })
}

fn tenant<'a>(relay: Option<&'a str>, signer: Option<&'a str>) -> Tenant<'a> {
    Tenant {
        relay_url: relay,
        signer_pubkey: signer,
    }
}

#[test]
fn a_scope_mismatch_names_the_session_action_it_refused() {
    let payload = scoped_payload("wss://tenant-b.example", "bb22");
    for (action, relay_text, signer_text) in [
        (
            Action::End,
            "active community changed before the session was ended",
            "active identity changed before the session was ended",
        ),
        (
            Action::Recover(RecoveryMode::Fresh),
            "active community changed before the session was recovered",
            "active identity changed before the session was recovered",
        ),
    ] {
        let error = assert_payload_scope(
            &payload,
            tenant(Some("wss://tenant-a.example"), None),
            action,
        )
        .unwrap_err();
        assert!(error.starts_with(relay_text), "{error}");
        let error = assert_payload_scope(&payload, tenant(None, Some("aa11")), action).unwrap_err();
        assert!(error.starts_with(signer_text), "{error}");
        assert!(!error.contains("not sent"), "{error}");
    }
}

#[test]
fn matching_scope_and_signer_pass_on_the_rebuilt_payload() {
    for action in [
        Action::Deploy,
        Action::End,
        Action::Recover(RecoveryMode::Checkpoint),
    ] {
        assert_payload_scope(
            &scoped_payload("wss://tenant-a.example", "aa11"),
            tenant(Some("wss://tenant-a.example"), Some("AA11")),
            action,
        )
        .unwrap();
    }
}

#[test]
fn relay_switch_during_the_lock_wait_fails_closed() {
    // Round-8 P1: a stale Projects-A start waited behind another deploy;
    // the rebuild resolved tenant B. The payload actually invoked must be
    // refused — the pre-lock snapshot its caller validated is irrelevant.
    let error = assert_payload_scope(
        &scoped_payload("wss://tenant-b.example", "aa11"),
        tenant(Some("wss://tenant-a.example"), Some("aa11")),
        Action::Deploy,
    )
    .unwrap_err();
    assert!(error.contains("active community changed"), "{error}");
}

#[test]
fn same_relay_identity_switch_during_the_lock_wait_fails_closed() {
    // Same relay, different owner: an identity switch alone must also be
    // refused — the rebuilt launch.owner_pubkey belongs to a tenant the
    // caller never validated.
    let error = assert_payload_scope(
        &scoped_payload("wss://tenant-a.example", "bb22"),
        tenant(Some("wss://tenant-a.example"), Some("aa11")),
        Action::Deploy,
    )
    .unwrap_err();
    assert!(error.contains("active identity changed"), "{error}");
}

#[test]
fn scoped_caller_with_an_unverifiable_payload_fails_closed() {
    let payload = serde_json::json!({});
    let relay_error = assert_payload_scope(
        &payload,
        tenant(Some("wss://tenant-a.example"), None),
        Action::Deploy,
    )
    .unwrap_err();
    assert!(relay_error.contains("no relay"), "{relay_error}");
    let signer_error =
        assert_payload_scope(&payload, tenant(None, Some("aa11")), Action::Deploy).unwrap_err();
    assert!(signer_error.contains("no owner identity"), "{signer_error}");
}

#[test]
fn unscoped_callers_deploy_any_payload() {
    assert_payload_scope(
        &scoped_payload("wss://anywhere.example", "cc33"),
        tenant(None, None),
        Action::Deploy,
    )
    .unwrap();
    assert_payload_scope(&serde_json::json!({}), tenant(None, None), Action::Deploy).unwrap();
    assert_payload_scope(
        &serde_json::json!({}),
        tenant(Some("  "), Some("")),
        Action::Deploy,
    )
    .unwrap();
}

#[test]
fn session_actions_require_both_tenant_parts() {
    for (relay, signer) in [
        (None, Some("aa11")),
        (Some("wss://tenant-a.example"), None),
        (Some(" "), Some("aa11")),
        (Some("wss://tenant-a.example"), Some("")),
        (None, None),
    ] {
        let error = tenant(relay, signer).require_both().unwrap_err();
        assert!(
            error.message.contains("require the community and identity"),
            "{error}"
        );
    }
    tenant(Some("wss://tenant-a.example"), Some("aa11"))
        .require_both()
        .unwrap();
}

// ── apply_replay_floor: publish-first floor threading into the payload ──

fn launch_payload() -> serde_json::Value {
    serde_json::json!({
        "launch": {
            "env": { "KEEP_ME": "yes" },
            "policy_env": { "BUZZ_ACP_LAZY_POOL": "true" },
        },
    })
}

#[test]
fn caller_replay_floor_rides_launch_policy_env() {
    // A publish-first mention send's floor must reach the remote harness
    // as BUZZ_ACP_REPLAY_FLOOR, exactly like a local spawn's env.
    let mut payload = launch_payload();
    apply_replay_floor(&mut payload, Some(1_756_600_000));
    assert_eq!(
        payload["launch"]["policy_env"]["BUZZ_ACP_REPLAY_FLOOR"],
        "1756600000"
    );
    assert_eq!(payload["launch"]["env"]["KEEP_ME"], "yes");
    assert_eq!(
        payload["launch"]["policy_env"]["BUZZ_ACP_LAZY_POOL"],
        "true"
    );
}

#[test]
fn caller_replay_floor_strips_user_env_shadow() {
    // launch.env later-wins over policy_env in the remote three-tier
    // model; a persisted user floor must not shadow this send's floor.
    let mut payload = launch_payload();
    payload["launch"]["env"]["BUZZ_ACP_REPLAY_FLOOR"] = "1".into();
    payload["launch"]["env"]["buzz_acp_replay_floor"] = "2".into();
    apply_replay_floor(&mut payload, Some(42));
    assert_eq!(
        payload["launch"]["policy_env"]["BUZZ_ACP_REPLAY_FLOOR"],
        "42"
    );
    assert!(payload["launch"]["env"]["BUZZ_ACP_REPLAY_FLOOR"].is_null());
    assert!(payload["launch"]["env"]["buzz_acp_replay_floor"].is_null());
    assert_eq!(payload["launch"]["env"]["KEEP_ME"], "yes");
}

#[test]
fn no_caller_floor_leaves_payload_untouched() {
    // Create-flow deploys and plain redeploys carry no floor: user env
    // passthrough stands and no stale floor is invented.
    let mut payload = launch_payload();
    payload["launch"]["env"]["BUZZ_ACP_REPLAY_FLOOR"] = "1".into();
    let before = payload.clone();
    apply_replay_floor(&mut payload, None);
    assert_eq!(payload, before);
}

#[test]
fn replay_floor_tolerates_payload_without_launch() {
    let mut payload = serde_json::json!({});
    apply_replay_floor(&mut payload, Some(42));
    assert_eq!(payload, serde_json::json!({}));
}

#[test]
fn replay_floor_creates_missing_policy_env() {
    let mut payload = serde_json::json!({ "launch": {} });
    apply_replay_floor(&mut payload, Some(42));
    assert_eq!(
        payload["launch"]["policy_env"]["BUZZ_ACP_REPLAY_FLOOR"],
        "42"
    );
}
