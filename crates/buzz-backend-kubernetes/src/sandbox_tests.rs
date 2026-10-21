use super::*;
use nostr::nips::nip19::ToBech32;

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
    let fp = pod::intent_template(&cfg, vec![]).fingerprint();
    let mut sandbox = build(&identity, &cfg, &scope, "owner", "generation", &fp).unwrap();
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
    verify(&sandbox, &identity, &scope, "owner", &fp).unwrap();
    sandbox
        .annotations_mut()
        .insert(OWNER.into(), "different-owner".into());
    assert!(verify(&sandbox, &identity, &scope, "owner", &fp).is_err());
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
    assert!(deploy(
        client.clone(),
        &identity,
        &cfg,
        &scope,
        "owner",
        other_relay
    )
    .await
    .unwrap_err()
    .contains("different relay"));
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
    assert!(error.contains("replaced"), "{error}");
    let sandboxes = api(client, &namespace)
        .list(&ListParams::default())
        .await
        .unwrap();
    assert_eq!(sandboxes.items.len(), 2, "one Sandbox per canonical thread");
    namespaces
        .delete(&namespace, &DeleteParams::default())
        .await
        .unwrap();
}
