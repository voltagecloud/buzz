use super::*;
use buzz_backend_kubernetes::lifecycle;
use buzz_backend_kubernetes::lifecycle::LIFECYCLE_ENDED;
use nostr::nips::nip19::ToBech32;

#[path = "sandbox_fake.rs"]
mod fake;

#[test]
fn custom_image_is_inside_a_real_sandbox_with_a_downward_uid() {
    let identity = naming::AgentIdentity::from_nsec(
        &nostr::Keys::generate().secret_key().to_bech32().unwrap(),
    )
    .unwrap();
    let cfg = crate::config::parse(&json!({"namespace":"agents", "image":format!("example.com/agent@sha256:{}", "a".repeat(64)),
        "pod_options":{"active_deadline_seconds":3600,"workspace_size_limit":"50Gi"}})).unwrap();
    let scope = Options {
        channel_id: uuid::Uuid::new_v4().to_string(),
        thread_root: "c".repeat(64),
    };
    let fps = Fingerprints::new(&cfg, &BTreeMap::new());
    let new = NewSession {
        generation: "generation",
        checkpointing: false,
    };
    let mut sandbox = build(&identity, &cfg, &scope, "owner", &new, &fps.current).unwrap();
    assert_eq!(
        sandbox.types.as_ref().unwrap().api_version,
        "agents.x-k8s.io/v1beta1"
    );
    let spec = &sandbox.data["spec"]["podTemplate"]["spec"];
    assert_eq!(spec["containers"][0]["image"], cfg.image.as_str());
    assert_eq!(
        spec["containers"][0]["env"][0]["valueFrom"]["fieldRef"]["fieldPath"],
        "metadata.uid"
    );
    assert_eq!(spec["automountServiceAccountToken"], false);
    assert_eq!(spec["restartPolicy"], "Never");
    // One grace for both paths: it covers the checkpoint, and intent
    // fingerprints assume it.
    assert_eq!(
        spec["terminationGracePeriodSeconds"],
        crate::config::TERMINATION_GRACE_SECONDS
    );
    assert_eq!(spec["volumes"][0]["emptyDir"]["sizeLimit"], "50Gi");
    assert_eq!(spec["containers"][0]["terminationMessagePolicy"], "File");
    assert_eq!(annotation(&sandbox, CHECKPOINTING), Some("disabled"));
    assert_eq!(annotation(&sandbox, RESTORE_CHECKPOINT), None);
    verify_identity(&sandbox, &identity, &scope, "owner").unwrap();
    verify_live(&sandbox, &fps).unwrap();
    sandbox
        .annotations_mut()
        .insert(OWNER.into(), "different-owner".into());
    assert!(verify_identity(&sandbox, &identity, &scope, "owner").is_err());
}

#[test]
fn options_parse_accepts_only_a_canonical_thread_scope() {
    let channel = "0f8fad5b-d9cb-469f-a165-70867728950e";
    let root = "a".repeat(64);
    let parse = |sandbox: serde_json::Value| Options::parse(&json!({ "sandbox": sandbox }));
    assert!(Options::parse(&json!({})).unwrap().is_none());
    assert!(parse(json!(false)).unwrap().is_none());
    let scope = parse(json!({"channel_id": channel, "thread_root": root}))
        .unwrap()
        .unwrap();
    assert_eq!(
        (scope.channel_id.as_str(), scope.thread_root.as_str()),
        (channel, root.as_str())
    );
    for (sandbox, error) in [
        (json!(true), "requires channel_id"),
        (json!({"channel_id": channel}), "requires channel_id"),
        (
            json!({"channel_id": channel, "thread_root": root, "extra": 1}),
            "requires channel_id",
        ),
        (
            json!({"channel_id": channel.to_uppercase(), "thread_root": root}),
            "canonical UUID",
        ),
        (
            json!({"channel_id": channel.replace('-', ""), "thread_root": root}),
            "canonical UUID",
        ),
        (
            json!({"channel_id": channel, "thread_root": "A".repeat(64)}),
            "lowercase 64-character",
        ),
        (
            json!({"channel_id": channel, "thread_root": "a".repeat(63)}),
            "lowercase 64-character",
        ),
        (
            json!({"channel_id": channel, "thread_root": "g".repeat(64)}),
            "lowercase 64-character",
        ),
    ] {
        let refused = parse(sandbox.clone()).unwrap_err();
        assert!(refused.contains(error), "{sandbox}: {refused}");
    }
}

fn identity() -> naming::AgentIdentity {
    naming::AgentIdentity::from_nsec(&nostr::Keys::generate().secret_key().to_bech32().unwrap())
        .unwrap()
}

#[test]
fn account_setup_refuses_any_other_mode_runtime_or_permission() {
    let cfg = crate::config::parse(&json!({"namespace":"agents",
        "image":format!("example.com/agent@sha256:{}", "a".repeat(64))}))
    .unwrap();
    for (key, value) in [
        ("BUZZ_SETUP_MODE", "codex-device-v2"),
        ("BUZZ_ACP_AGENT_COMMAND", "claude-agent-acp"),
        ("BUZZ_ACP_PERMISSION_MODE", "default"),
    ] {
        let mut cfg = cfg.clone();
        let mut env = fixture_env();
        env.insert(key.into(), value.into());
        let before = env.clone();
        assert!(configure_setup(&mut cfg, &identity(), &mut env)
            .unwrap_err()
            .contains("codex-device-v1"));
        assert_eq!(env, before, "a refused setup must not change the launch");
        assert!(!cfg.pod_options.setup_pending);
    }
    let mut cfg = cfg.clone();
    let mut env = fixture_env();
    configure_setup(&mut cfg, &identity(), &mut env).unwrap();
    assert!(cfg.pod_options.setup_pending);
    assert!(!env.contains_key("BUZZ_SETUP_POD_UID"));
}

#[test]
fn session_env_requires_checkpoint_storage_unless_disabled() {
    let scope = Options {
        channel_id: uuid::Uuid::new_v4().to_string(),
        thread_root: "c".repeat(64),
    };
    let mut env = fixture_env();
    apply_session_env(&scope, &mut env).unwrap();
    assert_eq!(env["BUZZ_ACP_CHECKPOINT_ON_STOP"], "true");
    assert_eq!(env["BUZZ_ACP_RESPOND_TO"], "owner");

    for missing in ["BUZZ_CHECKPOINT_BUCKET", "BUZZ_CHECKPOINT_PREFIX"] {
        let mut env = fixture_env();
        env.remove(missing);
        let refused = apply_session_env(&scope, &mut env).unwrap_err();
        assert!(
            refused.contains("BUZZ_SANDBOX_CHECKPOINT=disabled"),
            "{refused}"
        );
        // A blank value would let the launch through and fail the stop save.
        for blank in ["", "  "] {
            let mut env = fixture_env();
            env.insert(missing.into(), blank.into());
            assert!(
                apply_session_env(&scope, &mut env).is_err(),
                "{missing}={blank:?} must be refused"
            );
            assert!(!env.contains_key("BUZZ_ACP_CHECKPOINT_ON_STOP"));
        }
    }

    let mut env = fixture_env();
    env.remove("BUZZ_CHECKPOINT_BUCKET");
    env.insert(CHECKPOINT_OPT_OUT.into(), "disabled".into());
    apply_session_env(&scope, &mut env).unwrap();
    assert!(!env.contains_key("BUZZ_ACP_CHECKPOINT_ON_STOP"));
    // The opt-out also wins over configured storage.
    let mut env = fixture_env();
    env.insert(CHECKPOINT_OPT_OUT.into(), "disabled".into());
    apply_session_env(&scope, &mut env).unwrap();
    assert!(!env.contains_key("BUZZ_ACP_CHECKPOINT_ON_STOP"));
}

fn fixture_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("BUZZ_RELAY_URL".into(), "wss://fixture.invalid".into()),
        ("BUZZ_CHECKPOINT_BUCKET".into(), "fixture-bucket".into()),
        ("BUZZ_CHECKPOINT_PREFIX".into(), "fixture/".into()),
        ("BUZZ_SETUP_MODE".into(), "codex-device-v1".into()),
        ("BUZZ_ACP_AGENT_COMMAND".into(), "codex-acp".into()),
        (
            "BUZZ_ACP_PERMISSION_MODE".into(),
            "agent-full-access".into(),
        ),
        (
            "BUZZ_SETUP_AGENT_PUBKEY".into(),
            "stale-caller-value".into(),
        ),
        ("BUZZ_SETUP_POD_UID".into(), "stale-caller-value".into()),
    ])
}

#[tokio::test]
#[ignore = "requires the isolated kind-remote-agent-check cluster and fixture image"]
async fn live_controller_reuses_scopes_and_refuses_lost_workspaces() {
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::DeleteParams;
    use kube::config::{KubeConfigOptions, Kubeconfig};

    let config =
        Kubeconfig::read_from(std::env::var("BUZZ_SANDBOX_TEST_KUBECONFIG").unwrap()).unwrap();
    let config = kube::Config::from_custom_kubeconfig(
        config,
        &KubeConfigOptions {
            context: Some("kind-remote-agent-check".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        config.cluster_url.host(),
        Some("127.0.0.1"),
        "never use a shared cluster for this test"
    );
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = Client::try_from(config).unwrap();
    let namespace = format!(
        "buzz-agent-staging-test-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let namespaces: Api<Namespace> = Api::all(client.clone());
    namespaces.create(&PostParams::default(), &serde_json::from_value(json!({
        "metadata":{"name":namespace,"labels":{"pod-security.kubernetes.io/enforce":"restricted"}}
    })).unwrap()).await.unwrap();
    let cfg = crate::config::parse(&json!({
        "namespace":namespace, "image":std::env::var("BUZZ_SANDBOX_TEST_IMAGE").unwrap(),
        "cpu_request":"10m", "memory_request":"16Mi", "cpu_limit":"100m", "memory_limit":"32Mi",
        "pod_options":{"active_deadline_seconds":600}
    }))
    .unwrap();
    let identity = naming::AgentIdentity::from_nsec(
        &nostr::Keys::generate().secret_key().to_bech32().unwrap(),
    )
    .unwrap();
    let scope = Options {
        channel_id: uuid::Uuid::new_v4().to_string(),
        thread_root: "a".repeat(64),
    };
    let (first, concurrent) = tokio::join!(
        deploy(
            client.clone(),
            &identity,
            &cfg,
            &scope,
            "owner",
            fixture_env()
        ),
        deploy(
            client.clone(),
            &identity,
            &cfg,
            &scope,
            "owner",
            fixture_env()
        ),
    );
    let first = first.unwrap();
    assert_eq!(
        concurrent.unwrap(),
        first,
        "concurrent mentions reuse one Sandbox"
    );
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &namespace);
    let before = secrets.list(&ListParams::default()).await.unwrap();
    assert_eq!(before.items.len(), 1);
    let data = before.items[0].data.as_ref().unwrap();
    assert_eq!(
        data["BUZZ_SETUP_AGENT_PUBKEY"].0,
        identity.pubkey_hex().as_bytes()
    );
    assert_eq!(data["BUZZ_SETUP_NONCE"].0.len(), 32);
    assert_eq!(data["BUZZ_MANAGED_AGENT_START_NONCE"].0.len(), 32);
    assert!(!data.contains_key("BUZZ_SETUP_POD_UID"));
    assert_eq!(
        deploy(
            client.clone(),
            &identity,
            &cfg,
            &scope,
            "owner",
            fixture_env()
        )
        .await
        .unwrap(),
        first
    );
    let after = secrets.list(&ListParams::default()).await.unwrap();
    assert_eq!(after.items.len(), 1);
    assert_eq!(
        after.items[0].data, before.items[0].data,
        "retry must preserve setup binding and deadline"
    );
    assert!(deploy(
        client.clone(),
        &identity,
        &cfg,
        &scope,
        "other-owner",
        fixture_env()
    )
    .await
    .is_err());
    let mut other_relay = fixture_env();
    other_relay.insert("BUZZ_RELAY_URL".into(), "wss://other.invalid".into());
    assert_eq!(
        deploy(
            client.clone(),
            &identity,
            &cfg,
            &scope,
            "owner",
            other_relay
        )
        .await
        .unwrap_err()
        .code,
        Some(ErrorCode::RelayDiffers)
    );
    let second_scope = Options {
        channel_id: scope.channel_id.clone(),
        thread_root: "b".repeat(64),
    };
    let second = deploy(
        client.clone(),
        &identity,
        &cfg,
        &second_scope,
        "owner",
        fixture_env(),
    )
    .await
    .unwrap();
    assert_ne!(first, second);
    let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let original = pods.get(&first).await.unwrap();
    let pod_uid = original.metadata.uid.unwrap();
    pods.delete(
        &first,
        &DeleteParams {
            grace_period_seconds: Some(0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut replacement = None;
    for _ in 0..30 {
        if let Some(pod) = pods.get_opt(&first).await.unwrap() {
            if pod.metadata.uid.as_deref() != Some(&pod_uid) {
                replacement = Some(pod);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        replacement.is_some(),
        "upstream controller recreates the Pod"
    );
    let error = deploy(
        client.clone(),
        &identity,
        &cfg,
        &scope,
        "owner",
        fixture_env(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, Some(ErrorCode::PodReplaced), "{error}");
    let sandboxes = api(client.clone(), &namespace)
        .list(&ListParams::default())
        .await
        .unwrap();
    assert_eq!(sandboxes.items.len(), 2, "one Sandbox per canonical thread");

    // Lifecycle: stop suspends the session; once tombstoned (the reaper's job,
    // simulated here) an ordinary deploy is refused.
    let (_, state) = stop(
        client.clone(),
        &identity,
        &namespace,
        &second_scope,
        "owner",
    )
    .await
    .unwrap();
    assert_eq!(state, StopState::Ending);
    let sandboxes = api(client.clone(), &namespace);
    let old = sandboxes.get(&second).await.unwrap();
    assert_eq!(old.data["spec"]["operatingMode"], "Suspended");
    let tombstone = json!({"metadata": {"annotations": {
        (LIFECYCLE): LIFECYCLE_ENDED, (lifecycle::CHECKPOINT): lifecycle::CHECKPOINT_MISSING,
        (lifecycle::ENDED_AT): "2026-01-01T00:00:00Z"}}});
    sandboxes
        .patch(&second, &PatchParams::default(), &Patch::Merge(&tombstone))
        .await
        .unwrap();
    let ended = deploy(
        client.clone(),
        &identity,
        &cfg,
        &second_scope,
        "owner",
        fixture_env(),
    )
    .await
    .unwrap_err();
    assert_eq!(ended.code, Some(ErrorCode::SessionEnded), "{ended}");
    namespaces
        .delete(&namespace, &DeleteParams::default())
        .await
        .unwrap();
}

// ── Fake-apiserver tests: the real deploy/stop paths over kube::Client ──────

fn fake_identity() -> naming::AgentIdentity {
    naming::AgentIdentity::from_nsec(&nostr::Keys::generate().secret_key().to_bech32().unwrap())
        .unwrap()
}

fn fake_cfg() -> ProviderConfig {
    crate::config::parse(&json!({"namespace":"agents",
        "image":format!("example.com/agent@sha256:{}", "a".repeat(64)),
        "pod_options":{"active_deadline_seconds":3600}}))
    .unwrap()
}

fn fake_scope() -> Options {
    Options {
        channel_id: "0b6c1f3e-5a7d-4c2b-9e8f-1a2b3c4d5e6f".into(),
        thread_root: "d".repeat(64),
    }
}

fn plain_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("BUZZ_RELAY_URL".into(), "wss://fixture.invalid".into()),
        ("BUZZ_CHECKPOINT_BUCKET".into(), "bucket".into()),
        ("BUZZ_CHECKPOINT_PREFIX".into(), "dev/".into()),
    ])
}

fn secret_env(state: &fake::Shared, key: &str) -> Vec<String> {
    state
        .lock()
        .unwrap()
        .secrets
        .values()
        .map(|secret| {
            let secret: Secret = serde_json::from_value(secret.clone()).unwrap();
            String::from_utf8(secret.data.unwrap()[key].0.clone()).unwrap()
        })
        .collect()
}

fn sandbox_json(state: &fake::Shared, name: &str) -> serde_json::Value {
    state.lock().unwrap().sandboxes[name].clone()
}

#[tokio::test(start_paused = true)]
async fn fresh_deploy_binds_and_always_carries_an_empty_restore_key() {
    let state = fake::Shared::default();
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    let name = deploy(
        fake::client(&state),
        &identity,
        &cfg,
        &scope,
        "owner",
        plain_env(),
    )
    .await
    .unwrap();
    let sandbox = sandbox_json(&state, &name);
    let annotations = &sandbox["metadata"]["annotations"];
    assert_eq!(annotations[CHECKPOINTING], "enabled");
    let initial_pod_uid = annotations[INITIAL_POD].as_str().unwrap();
    assert_eq!(
        secret_env(&state, "BUZZ_SANDBOX_INITIAL_POD_UID"),
        [initial_pod_uid]
    );
    let secret_owner_uids: Vec<_> = state
        .lock()
        .unwrap()
        .secrets
        .values()
        .map(|secret| {
            secret["metadata"]["ownerReferences"][0]["uid"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(secret_owner_uids, [initial_pod_uid]);
    assert!(annotations.get(RESTORE_CHECKPOINT).is_none());
    assert_eq!(secret_env(&state, RESTORE_KEY_ENV), [""]);
    assert_eq!(
        secret_env(&state, "BUZZ_MANAGED_AGENT_START_NONCE"),
        [annotations[GENERATION].as_str().unwrap()]
    );
    // A repeat deploy reuses the session.
    assert_eq!(
        deploy(
            fake::client(&state),
            &identity,
            &cfg,
            &scope,
            "owner",
            plain_env()
        )
        .await
        .unwrap(),
        name
    );
    assert_eq!(state.lock().unwrap().secrets.len(), 1);
}

/// Every identity and liveness refusal for an existing Sandbox, through the
/// real deploy path. A refused deploy writes nothing: no Secret, and the
/// Sandbox (including its resourceVersion) is unchanged.
#[tokio::test(start_paused = true)]
async fn deploy_refuses_a_sandbox_that_is_not_a_reusable_live_session() {
    type Mutate = fn(&mut serde_json::Value);
    fn set(sandbox: &mut serde_json::Value, key: &str, value: &str) {
        sandbox["metadata"]["annotations"][key] = value.into();
    }
    let cases: [(&str, Mutate, ErrorCode, &str); 10] = [
        (
            "ended",
            |s| {
                set(s, lifecycle::LIFECYCLE, lifecycle::LIFECYCLE_ENDED);
                set(s, ENDED_REASON, "lost");
            },
            ErrorCode::SessionEnded,
            "Sandbox session ended (lost); explicit recovery is required",
        ),
        (
            "ended with an unknown reason",
            |s| {
                set(s, lifecycle::LIFECYCLE, lifecycle::LIFECYCLE_ENDED);
                set(s, ENDED_REASON, "see https://evil.invalid");
            },
            ErrorCode::SessionEnded,
            "Sandbox session ended; explicit recovery is required",
        ),
        (
            "ending",
            |s| set(s, lifecycle::LIFECYCLE, lifecycle::LIFECYCLE_ENDING),
            ErrorCode::SessionEnding,
            "Sandbox session is ending; recover it explicitly once it has ended",
        ),
        (
            "unknown lifecycle",
            |s| set(s, lifecycle::LIFECYCLE, "paused"),
            ErrorCode::LifecycleUnknown,
            "Sandbox has a lifecycle this provider does not know; update Buzz",
        ),
        (
            "deletionTimestamp",
            |s| s["metadata"]["deletionTimestamp"] = "2026-01-01T00:00:00Z".into(),
            ErrorCode::SessionDeleting,
            "Sandbox is being deleted; retry once it is gone",
        ),
        (
            "Suspended without lifecycle",
            |s| s["spec"]["operatingMode"] = "Suspended".into(),
            ErrorCode::SessionNotRunning,
            "Sandbox is not an active managed session; explicit recovery is required",
        ),
        (
            "not Running",
            |s| s["spec"]["operatingMode"] = "Hibernated".into(),
            ErrorCode::SessionNotRunning,
            "Sandbox is not an active managed session; explicit recovery is required",
        ),
        (
            "binding-version mismatch",
            |s| s["metadata"]["labels"][naming::LABEL_BINDING_VERSION] = "1".into(),
            ErrorCode::IdentityMismatch,
            "Sandbox belongs to a different agent, owner, or thread",
        ),
        (
            "fingerprint mismatch",
            |s| set(s, naming::ANNOTATION_CREATE_INTENT, &"0".repeat(64)),
            ErrorCode::ConfigDiffers,
            "Sandbox launch configuration differs; end the session, then explicit recovery is required",
        ),
        (
            "owner mismatch",
            |s| set(s, OWNER, "other-owner"),
            ErrorCode::IdentityMismatch,
            "Sandbox belongs to a different agent, owner, or thread",
        ),
    ];
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    let name = scope.name(&identity);
    for (case, mutate, code, message) in cases {
        let state = fake::Shared::default();
        let redeploy = || {
            deploy(
                fake::client(&state),
                &identity,
                &cfg,
                &scope,
                "owner",
                plain_env(),
            )
        };
        redeploy().await.unwrap();
        let before = {
            let mut guard = state.lock().unwrap();
            // Without its Secret, an accepted Sandbox would be written to.
            guard.secrets.clear();
            let sandbox = guard.sandboxes.get_mut(&name).unwrap();
            mutate(sandbox);
            sandbox.clone()
        };
        let error = redeploy().await.unwrap_err();
        assert_eq!(
            (error.code, error.message.as_str()),
            (Some(code), message),
            "{case}"
        );
        let guard = state.lock().unwrap();
        assert!(guard.secrets.is_empty(), "{case}: wrote a Secret");
        assert_eq!(guard.sandboxes[&name], before, "{case}: wrote the Sandbox");
    }
}

/// The medium bug: bound to the first Pod, Secret never written, container
/// never started. A later deploy must finish the binding, not strand it.
#[tokio::test(start_paused = true)]
async fn a_bound_session_without_its_secret_is_resumed() {
    let state = fake::Shared::default();
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    deploy(
        fake::client(&state),
        &identity,
        &cfg,
        &scope,
        "owner",
        plain_env(),
    )
    .await
    .unwrap();
    let name = scope.name(&identity);
    let (generation, pod_uid) = {
        let mut guard = state.lock().unwrap();
        guard.secrets.clear();
        let sandbox = &guard.sandboxes[&name];
        (
            sandbox["metadata"]["annotations"][GENERATION].clone(),
            guard.pods[&name].clone(),
        )
    };
    deploy(
        fake::client(&state),
        &identity,
        &cfg,
        &scope,
        "owner",
        plain_env(),
    )
    .await
    .unwrap();
    let guard = state.lock().unwrap();
    let (secret_name, secret) = guard.secrets.iter().next().unwrap();
    assert!(secret_name.ends_with(generation.as_str().unwrap()));
    assert_eq!(secret["metadata"]["ownerReferences"][0]["uid"], pod_uid);
    assert_eq!(
        sandbox_json_locked(&guard, &name)["metadata"]["annotations"][GENERATION],
        generation
    );
}

fn sandbox_json_locked(state: &fake::State, name: &str) -> serde_json::Value {
    state.sandboxes[name].clone()
}

#[tokio::test(start_paused = true)]
async fn a_secret_create_failure_preserves_the_binding_and_the_retry_succeeds() {
    assert_secret_create_retry(false).await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_secret_create_response_preserves_the_live_session() {
    assert_secret_create_retry(true).await;
}

async fn assert_secret_create_retry(stored_before_error: bool) {
    let state = fake::Shared::default();
    if stored_before_error {
        state.lock().unwrap().fail_secret_create_responses = 1;
    } else {
        state.lock().unwrap().fail_secret_creates = 1;
    }
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    let error = deploy(
        fake::client(&state),
        &identity,
        &cfg,
        &scope,
        "owner",
        plain_env(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, None, "{error}");
    assert!(error.message.ends_with("retry deploy"), "{error}");
    let name = scope.name(&identity);
    let sandbox = sandbox_json(&state, &name);
    let pod_uid = state.lock().unwrap().pods[&name].clone();
    assert_eq!(
        sandbox["metadata"]["annotations"][INITIAL_POD], pod_uid,
        "an uncertain Secret write must not discard the original Pod binding"
    );
    assert_eq!(
        state.lock().unwrap().secrets.len(),
        usize::from(stored_before_error)
    );
    deploy(
        fake::client(&state),
        &identity,
        &cfg,
        &scope,
        "owner",
        plain_env(),
    )
    .await
    .unwrap();
    assert_eq!(sandbox_json(&state, &name), sandbox);
    let guard = state.lock().unwrap();
    assert_eq!(guard.pods[&name], pod_uid);
    assert_eq!(guard.secrets.len(), 1);
    let secret = guard.secrets.values().next().unwrap();
    assert_eq!(secret["metadata"]["ownerReferences"][0]["uid"], pod_uid);
}

#[test]
fn the_restore_key_is_fingerprinted_by_name_not_value() {
    let cfg = fake_cfg();
    let scope = fake_scope();
    let mut plain = plain_env();
    apply_session_env(&scope, &mut plain).unwrap();
    assert_eq!(plain[RESTORE_KEY_ENV], "");
    let mut restored = plain.clone();
    restored.insert(RESTORE_KEY_ENV.into(), "dev/g/x.tar.gz".into());
    assert_eq!(
        pod::intent_template(&cfg, plain.keys().cloned()).fingerprint(),
        pod::intent_template(&cfg, restored.keys().cloned()).fingerprint()
    );
    let mut without = plain.clone();
    without.remove(RESTORE_KEY_ENV);
    assert_ne!(
        pod::intent_template(&cfg, plain.keys().cloned()).fingerprint(),
        pod::intent_template(&cfg, without.keys().cloned()).fingerprint()
    );
}

/// Sandboxes created before the restore key existed carry a fingerprint
/// without it. Mixed desktop versions must keep reusing them; any other
/// fingerprint is still a configuration change.
#[tokio::test(start_paused = true)]
async fn a_sandbox_from_before_the_restore_key_is_still_reused() {
    let state = fake::Shared::default();
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    let name = deploy(
        fake::client(&state),
        &identity,
        &cfg,
        &scope,
        "owner",
        plain_env(),
    )
    .await
    .unwrap();
    let mut env = plain_env();
    apply_session_env(&scope, &mut env).unwrap();
    let fingerprints = Fingerprints::new(&cfg, &env);
    assert_ne!(fingerprints.legacy, fingerprints.current);
    let set_intent = |value: &str| {
        state.lock().unwrap().sandboxes.get_mut(&name).unwrap()["metadata"]["annotations"]
            [naming::ANNOTATION_CREATE_INTENT] = value.into();
    };
    set_intent(fingerprints.legacy.as_str());
    let redeploy = || {
        deploy(
            fake::client(&state),
            &identity,
            &cfg,
            &scope,
            "owner",
            plain_env(),
        )
    };
    assert_eq!(redeploy().await.unwrap(), name);
    set_intent(&"0".repeat(64));
    assert_eq!(
        redeploy().await.unwrap_err().code,
        Some(ErrorCode::ConfigDiffers)
    );
}

/// Deploy once, then drop the launch Secret: a bound session whose
/// credentials were never written.
async fn unbound_credentials(
    identity: &naming::AgentIdentity,
    cfg: &ProviderConfig,
    scope: &Options,
) -> (fake::Shared, String) {
    let state = fake::Shared::default();
    let name = deploy(
        fake::client(&state),
        identity,
        cfg,
        scope,
        "owner",
        plain_env(),
    )
    .await
    .unwrap();
    state.lock().unwrap().secrets.clear();
    (state, name)
}

/// A container that ever ran without its Secret is not resumed: writing
/// credentials now would hand them to a process that already started.
#[tokio::test(start_paused = true)]
async fn a_started_session_without_credentials_is_not_resumed() {
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    for started in [
        json!({"restartCount": 1}),
        json!({"lastState": {"terminated": {"exitCode": 1}}}),
        json!({"state": {"running": {}}}),
    ] {
        let (state, _) = unbound_credentials(&identity, &cfg, &scope).await;
        state.lock().unwrap().container_status = Some(started.clone());
        let error = deploy(
            fake::client(&state),
            &identity,
            &cfg,
            &scope,
            "owner",
            plain_env(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, Some(ErrorCode::CredentialsMissing), "{started}");
        assert!(state.lock().unwrap().secrets.is_empty(), "{started}");
    }
}

/// The restore key in the Secret is the Sandbox's, whatever the request
/// carries, so a resumed binding keeps the recovery it was created with.
#[tokio::test(start_paused = true)]
async fn the_restore_key_comes_from_the_sandbox_not_the_request() {
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    for recorded in [Some("dev/generation/checkpoint.tar.gz"), None] {
        let (state, name) = unbound_credentials(&identity, &cfg, &scope).await;
        if let Some(key) = recorded {
            state.lock().unwrap().sandboxes.get_mut(&name).unwrap()["metadata"]["annotations"]
                [RESTORE_CHECKPOINT] = key.into();
        }
        let mut env = plain_env();
        env.insert(RESTORE_KEY_ENV.into(), "dev/other/forged.tar.gz".into());
        deploy(fake::client(&state), &identity, &cfg, &scope, "owner", env)
            .await
            .unwrap();
        assert_eq!(
            secret_env(&state, RESTORE_KEY_ENV),
            [recorded.unwrap_or_default()]
        );
    }
}

/// Resuming stops before the reaper's bind timeout can end the session
/// under a deploy that is still writing credentials or waiting for startup.
#[tokio::test(start_paused = true)]
async fn a_binding_too_close_to_the_reaper_timeout_is_not_resumed() {
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    let window = BIND_TIMEOUT_SECS - DEPLOY_DEADLINE_SECS as i64 - CLOCK_SKEW_SECS;
    for (age, resumed) in [(window - 60, true), (window, false)] {
        let (state, name) = unbound_credentials(&identity, &cfg, &scope).await;
        let created = Utc::now() - chrono::Duration::seconds(age);
        state
            .lock()
            .unwrap()
            .pod_created
            .insert(name, created.to_rfc3339());
        let result = deploy(
            fake::client(&state),
            &identity,
            &cfg,
            &scope,
            "owner",
            plain_env(),
        )
        .await;
        if resumed {
            result.unwrap();
            assert_eq!(state.lock().unwrap().secrets.len(), 1);
        } else {
            assert_eq!(
                result.unwrap_err().code,
                Some(ErrorCode::BindingExpired),
                "{age}"
            );
            assert!(state.lock().unwrap().secrets.is_empty());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn stop_suspends_with_a_cas_patch_and_retries_conflicts() {
    let state = fake::Shared::default();
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    assert_eq!(
        stop(fake::client(&state), &identity, "agents", &scope, "owner")
            .await
            .unwrap()
            .1,
        StopState::Absent
    );
    let name = deploy(
        fake::client(&state),
        &identity,
        &cfg,
        &scope,
        "owner",
        plain_env(),
    )
    .await
    .unwrap();
    assert!(stop(
        fake::client(&state),
        &identity,
        "agents",
        &scope,
        "other-owner"
    )
    .await
    .is_err());
    state.lock().unwrap().conflict_patches = 1;
    let (agent_id, stopped) = stop(fake::client(&state), &identity, "agents", &scope, "owner")
        .await
        .unwrap();
    assert_eq!(
        (agent_id.as_str(), stopped),
        (name.as_str(), StopState::Ending)
    );
    {
        let guard = state.lock().unwrap();
        let patch = guard.patches.last().unwrap();
        assert!(patch["metadata"]["resourceVersion"].is_string(), "{patch}");
        assert_eq!(patch["spec"]["operatingMode"], "Suspended");
        assert_eq!(
            patch["metadata"]["annotations"][LIFECYCLE],
            LIFECYCLE_ENDING
        );
        assert_eq!(patch["metadata"]["annotations"][ENDED_REASON], "stopped");
        assert_eq!(guard.patches.len(), 2, "one conflict, one retry");
        assert!(!guard.pods.contains_key(&name), "Suspended removes the Pod");
    }
    assert_eq!(
        stop(fake::client(&state), &identity, "agents", &scope, "owner")
            .await
            .unwrap()
            .1,
        StopState::Ending
    );
    let ending = deploy(
        fake::client(&state),
        &identity,
        &cfg,
        &scope,
        "owner",
        plain_env(),
    )
    .await
    .unwrap_err();
    assert_eq!(ending.code, Some(ErrorCode::SessionEnding), "{ending}");
}

/// `stop` never rewrites a tombstone, nor a lifecycle this version does not
/// know: either would erase the record the reaper and recovery rely on.
#[tokio::test(start_paused = true)]
async fn stop_leaves_ended_and_unknown_lifecycles_untouched() {
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    let cases: [(&str, Result<StopState, ErrorCode>); 2] = [
        (LIFECYCLE_ENDED, Ok(StopState::Ended)),
        ("paused", Err(ErrorCode::LifecycleUnknown)),
    ];
    for (lifecycle, expected) in cases {
        let state = fake::Shared::default();
        let name = deploy(
            fake::client(&state),
            &identity,
            &cfg,
            &scope,
            "owner",
            plain_env(),
        )
        .await
        .unwrap();
        let before = {
            let mut guard = state.lock().unwrap();
            let sandbox = guard.sandboxes.get_mut(&name).unwrap();
            sandbox["metadata"]["annotations"][LIFECYCLE] = lifecycle.into();
            sandbox.clone()
        };
        let result = stop(fake::client(&state), &identity, "agents", &scope, "owner").await;
        assert_eq!(
            result
                .map(|(_, stopped)| stopped)
                .map_err(|e| e.code.unwrap()),
            expected,
            "{lifecycle}"
        );
        let guard = state.lock().unwrap();
        assert!(guard.patches.is_empty(), "{lifecycle}: patched");
        assert_eq!(guard.sandboxes[&name], before, "{lifecycle}");
    }
}

/// A Sandbox that keeps changing makes `stop` back off between bounded
/// attempts, then give up.
#[tokio::test(start_paused = true)]
async fn stop_backs_off_between_conflicts_and_gives_up() {
    let state = fake::Shared::default();
    let (identity, cfg, scope) = (fake_identity(), fake_cfg(), fake_scope());
    deploy(
        fake::client(&state),
        &identity,
        &cfg,
        &scope,
        "owner",
        plain_env(),
    )
    .await
    .unwrap();
    state.lock().unwrap().conflict_patches = usize::MAX;
    let started = tokio::time::Instant::now();
    let error = stop(fake::client(&state), &identity, "agents", &scope, "owner")
        .await
        .unwrap_err();
    assert_eq!(error.code, None, "{error}");
    assert_eq!(state.lock().unwrap().patches.len(), STOP_ATTEMPTS as usize);
    let minimum: u64 = (1..u64::from(STOP_ATTEMPTS)).sum::<u64>() * STOP_BACKOFF_MS;
    assert!(started.elapsed() >= Duration::from_millis(minimum));
}
