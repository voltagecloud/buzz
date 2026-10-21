use super::*;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

const OWNER: &str = "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11";
const RELAY: &str = "ws://127.0.0.1:9";

type Deployed = (Result<String, SessionError>, Value);

/// An in-memory host: one record per agent, read on every rebuild.
#[derive(Default)]
struct FakeSource {
    records: Mutex<HashMap<String, Invocation>>,
    locks: AgentLocks,
    rebuilds: Mutex<Vec<String>>,
    deployed: Mutex<Vec<Deployed>>,
    record_error: Mutex<Option<SessionError>>,
}

impl FakeSource {
    fn with(self, agent: &str, provider_id: &str, provider_config: Value) -> Self {
        self.records.lock().unwrap().insert(
            agent.to_string(),
            Invocation {
                target: ProviderTarget {
                    provider_id: provider_id.to_string(),
                    provider_config,
                    cached_binary: None,
                },
                payload: json!({
                    "relay_url": RELAY,
                    "launch": {"owner_pubkey": OWNER, "env": {}},
                }),
            },
        );
        self
    }

    fn saved_config(&self, agent: &str) -> Value {
        self.records.lock().unwrap()[agent]
            .target
            .provider_config
            .clone()
    }
}

impl InvocationSource for &FakeSource {
    fn target(&self, agent_pubkey: &str) -> Result<Option<ProviderTarget>, SessionError> {
        let records = self.records.lock().unwrap();
        let record = records
            .get(agent_pubkey)
            .ok_or_else(|| format!("agent {agent_pubkey} not found"))?;
        Ok(Some(record.target.clone()))
    }

    fn rebuild(&self, agent_pubkey: &str) -> Result<Invocation, SessionError> {
        assert!(
            self.locks.is_held(agent_pubkey),
            "the invocation must be rebuilt under the agent's lock"
        );
        self.rebuilds.lock().unwrap().push(agent_pubkey.to_string());
        let records = self.records.lock().unwrap();
        Ok(records
            .get(agent_pubkey)
            .ok_or_else(|| format!("agent {agent_pubkey} not found"))?
            .clone())
    }

    fn record_deploy(
        &self,
        _agent_pubkey: &str,
        result: &Result<String, SessionError>,
        payload: &Value,
    ) -> Result<(), SessionError> {
        self.deployed
            .lock()
            .unwrap()
            .push((result.clone(), payload.clone()));
        match self.record_error.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

struct Provider {
    dir: tempfile::TempDir,
}

impl Provider {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// Install `buzz-backend-{id}`: `info` answers with `ops`; every other
    /// request is captured and answered with `response`.
    fn install(&self, id: &str, ops: Value, response: Value) {
        let path = self.dir.path().join(format!("buzz-backend-{id}"));
        let info = json!({"ok": true, "name": id, "version": "1", "protocol_version": 1,
            "description": "Fixture provider", "config_schema": {}, "ops": ops});
        std::fs::write(&path, format!(
            "#!/bin/sh\nIFS= read -r request\ncase \"$request\" in\n*'\"op\":\"info\"'*) printf '%s\\n' '{info}' ;;\n*) printf '%s\\n' \"$request\" > '{}'\nprintf '%s\\n' '{response}' ;;\nesac\n",
            self.capture().display(),
        ))
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn capture(&self) -> std::path::PathBuf {
        self.dir.path().join("request.json")
    }

    /// The last request other than `info`, consumed.
    fn take_request(&self) -> Option<Value> {
        let text = std::fs::read_to_string(self.capture()).ok()?;
        std::fs::remove_file(self.capture()).unwrap();
        Some(serde_json::from_str(&text).unwrap())
    }

    fn sessions<'a>(&self, source: &'a FakeSource) -> RemoteSessions<&'a FakeSource> {
        RemoteSessions::new(
            source,
            source.locks.clone(),
            Discovery {
                dirs: vec![self.dir.path().to_path_buf()],
            },
            None,
        )
    }
}

fn thread() -> SessionScope {
    SessionScope::new(
        "f9ee507e-d03c-4570-baa6-02951c38e1cd".parse().unwrap(),
        "a".repeat(64),
    )
    .unwrap()
}

fn tenant<'a>(relay: &'a str, signer: &'a str) -> Tenant<'a> {
    Tenant {
        relay_url: Some(relay),
        signer_pubkey: Some(signer),
    }
}

const LIFECYCLE: &[&str] = &["info", "deploy", "stop", "recover"];

#[tokio::test]
async fn lifecycle_actions_enforce_scope_and_preserve_provider_codes() {
    let config = json!({"sandbox": true, "namespace": "agents"});
    let source = FakeSource::default().with("thread-agent", "kubernetes", config.clone());
    let provider = Provider::new();
    let sessions = provider.sessions(&source);
    let session = thread();
    provider.install(
        "kubernetes",
        json!(LIFECYCLE),
        json!({"ok": true, "agent_id": "remote", "state": "ended"}),
    );

    for (relay, signer, message) in [
        ("", OWNER, "require the community and identity"),
        (RELAY, " ", "require the community and identity"),
        ("ws://other.invalid", OWNER, "active community changed"),
        (RELAY, "bb22", "active identity changed"),
    ] {
        for error in [
            sessions
                .end("thread-agent", tenant(relay, signer), &session)
                .await
                .unwrap_err(),
            sessions
                .recover(
                    "thread-agent",
                    tenant(relay, signer),
                    &session,
                    RecoveryMode::Fresh,
                )
                .await
                .unwrap_err(),
        ] {
            assert!(error.message.contains(message), "{error}");
            assert!(error.message.contains("nothing was changed"), "{error}");
            assert_eq!(error.code, None);
        }
        assert!(
            provider.take_request().is_none(),
            "a refused action must not invoke the provider"
        );
    }
    assert!(source.deployed.lock().unwrap().is_empty());

    for (wire, state) in [
        ("ending", StopState::Ending),
        ("ended", StopState::Ended),
        ("absent", StopState::Absent),
    ] {
        provider.install(
            "kubernetes",
            json!(LIFECYCLE),
            json!({"ok": true, "agent_id": "remote", "state": wire}),
        );
        let ended = sessions
            .end("thread-agent", tenant(RELAY, OWNER), &session)
            .await
            .unwrap();
        assert_eq!(ended, state);
        let request = provider.take_request().unwrap();
        assert_eq!(request["op"], "stop");
        assert_eq!(
            request["provider_config"]["identity_policy"],
            json!({"agent_pubkey": "thread-agent", "owner_pubkey": OWNER})
        );
        assert_eq!(
            request["provider_config"]["sandbox"],
            json!({"channel_id": "f9ee507e-d03c-4570-baa6-02951c38e1cd", "thread_root": "a".repeat(64)})
        );
    }
    assert!(
        source.deployed.lock().unwrap().is_empty(),
        "ending a session records no deploy"
    );

    for (mode, wire) in [
        (RecoveryMode::Fresh, "fresh"),
        (RecoveryMode::Checkpoint, "checkpoint"),
    ] {
        provider.install(
            "kubernetes",
            json!(LIFECYCLE),
            json!({"ok": true, "agent_id": "remote"}),
        );
        sessions
            .recover("thread-agent", tenant(RELAY, OWNER), &session, mode)
            .await
            .unwrap();
        let request = provider.take_request().unwrap();
        assert_eq!(request["op"], "deploy");
        assert_eq!(
            request["provider_config"]["sandbox"]["recovery"],
            json!({"mode": wire})
        );
        assert_eq!(request["agent"]["launch"]["owner_pubkey"], OWNER);
        assert_eq!(
            source.saved_config("thread-agent"),
            config,
            "recovery must never reach the record"
        );
        let (result, payload) = source.deployed.lock().unwrap().pop().unwrap();
        assert_eq!(result, Ok("remote".to_string()));
        assert_eq!(payload, request["agent"]);
    }

    for code in [Some("no_checkpoint"), None] {
        let mut response = json!({"ok": false, "error": "fixture refusal"});
        if let Some(code) = code {
            response["code"] = json!(code);
        }
        provider.install("kubernetes", json!(LIFECYCLE), response);
        for error in [
            sessions
                .end("thread-agent", tenant(RELAY, OWNER), &session)
                .await
                .unwrap_err(),
            sessions
                .recover(
                    "thread-agent",
                    tenant(RELAY, OWNER),
                    &session,
                    RecoveryMode::Checkpoint,
                )
                .await
                .unwrap_err(),
        ] {
            assert_eq!(error.code.as_deref(), code);
            assert_eq!(error.message, "fixture refusal");
        }
        let (recorded, _) = source.deployed.lock().unwrap().pop().unwrap();
        assert_eq!(
            recorded.unwrap_err().code.as_deref(),
            code,
            "the recorded failure keeps its code"
        );
        assert_eq!(provider.take_request().unwrap()["op"], "deploy");
    }

    for (ops, expected) in [
        (json!(["info", "deploy", "stop"]), false),
        (json!(["info", "deploy", "recover"]), false),
        (json!(["info", "deploy"]), false),
        (json!(LIFECYCLE), true),
    ] {
        provider.install("kubernetes", ops.clone(), json!({"ok": true}));
        assert_eq!(
            sessions.supports_lifecycle("thread-agent").await.unwrap(),
            expected,
            "{ops}"
        );
        assert!(
            provider.take_request().is_none(),
            "supports_lifecycle runs only info"
        );
    }
}

#[tokio::test]
async fn deploy_binds_a_session_only_for_thread_sandboxes() {
    let sandbox = || json!({"sandbox": true, "namespace": "agents"});
    let source = FakeSource::default()
        .with("thread-agent", "kubernetes", sandbox())
        .with("other-agent", "other", sandbox());
    let provider = Provider::new();
    let sessions = provider.sessions(&source);
    let unscoped = Tenant::default();

    // No provider binary exists yet, so this error proves the refusal happens
    // before binary resolution.
    let refused = sessions
        .deploy("thread-agent", unscoped, None, None)
        .await
        .unwrap_err();
    assert!(refused.message.contains("Mention this agent"), "{refused}");
    assert!(source.deployed.lock().unwrap().is_empty());

    let ok = json!({"ok": true, "agent_id": "remote"});
    provider.install("kubernetes", json!(LIFECYCLE), ok.clone());
    sessions
        .deploy("thread-agent", unscoped, Some(&thread()), None)
        .await
        .unwrap();
    let request = provider.take_request().unwrap();
    assert_eq!(
        request["provider_config"],
        json!({
            "namespace": "agents",
            "sandbox": {
                "channel_id": "f9ee507e-d03c-4570-baa6-02951c38e1cd",
                "thread_root": "a".repeat(64),
            },
            "identity_policy": {"agent_pubkey": "thread-agent", "owner_pubkey": OWNER},
        })
    );
    assert_eq!(source.saved_config("thread-agent"), sandbox());

    // `sandbox: true` on another provider is ordinary configuration, and its
    // agents have no session to end or recover.
    provider.install("other", json!(LIFECYCLE), ok);
    sessions
        .deploy("other-agent", unscoped, None, None)
        .await
        .unwrap();
    assert_eq!(
        provider.take_request().unwrap()["provider_config"],
        sandbox()
    );
    let scoped = tenant(RELAY, OWNER);
    for error in [
        sessions
            .end("other-agent", scoped, &thread())
            .await
            .unwrap_err(),
        sessions
            .recover("other-agent", scoped, &thread(), RecoveryMode::Fresh)
            .await
            .unwrap_err(),
    ] {
        assert!(error.message.contains("Only thread workspaces"), "{error}");
    }
    assert!(provider.take_request().is_none());
    assert!(!sessions.supports_lifecycle("other-agent").await.unwrap());
}

#[tokio::test]
async fn a_scoped_deploy_is_checked_against_the_rebuilt_payload() {
    let source = FakeSource::default().with("agent", "other", json!({}));
    let provider = Provider::new();
    provider.install(
        "other",
        json!(LIFECYCLE),
        json!({"ok": true, "agent_id": "remote"}),
    );
    let sessions = provider.sessions(&source);

    // The record changed tenant after the caller captured its scope.
    source
        .records
        .lock()
        .unwrap()
        .get_mut("agent")
        .unwrap()
        .payload["relay_url"] = json!("wss://tenant-b.example");
    let error = sessions
        .deploy("agent", tenant(RELAY, OWNER), None, None)
        .await
        .unwrap_err();
    assert!(
        error.message.contains("active community changed"),
        "{error}"
    );
    assert!(provider.take_request().is_none());
    assert_eq!(*source.rebuilds.lock().unwrap(), ["agent"]);
}

#[tokio::test]
async fn the_replay_floor_reaches_only_the_invoked_payload() {
    let source = FakeSource::default().with("agent", "other", json!({}));
    let provider = Provider::new();
    provider.install(
        "other",
        json!(LIFECYCLE),
        json!({"ok": true, "agent_id": "remote"}),
    );
    let sessions = provider.sessions(&source);

    sessions
        .deploy("agent", Tenant::default(), None, Some(1_756_600_000))
        .await
        .unwrap();
    let request = provider.take_request().unwrap();
    assert_eq!(
        request["agent"]["launch"]["policy_env"][crate::REPLAY_FLOOR_ENV_VAR],
        "1756600000"
    );
    let (_, recorded) = source.deployed.lock().unwrap().pop().unwrap();
    assert_eq!(
        recorded, request["agent"],
        "the record sees what was deployed"
    );
    assert!(source.records.lock().unwrap()["agent"].payload["launch"]
        .get("policy_env")
        .is_none());

    sessions
        .deploy("agent", Tenant::default(), None, None)
        .await
        .unwrap();
    assert!(provider.take_request().unwrap()["agent"]["launch"]
        .get("policy_env")
        .is_none());
}

#[tokio::test]
async fn a_failed_record_write_is_returned_instead_of_the_deploy_result() {
    let source = FakeSource::default().with("agent", "other", json!({}));
    let provider = Provider::new();
    provider.install(
        "other",
        json!(LIFECYCLE),
        json!({"ok": true, "agent_id": "remote"}),
    );
    *source.record_error.lock().unwrap() = Some("agent agent not found".into());
    let sessions = provider.sessions(&source);

    let error = sessions
        .deploy("agent", Tenant::default(), None, None)
        .await
        .unwrap_err();
    assert_eq!(error.message, "agent agent not found");
    assert_eq!(source.deployed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn operations_on_one_agent_wait_for_its_lock() {
    let source =
        FakeSource::default()
            .with("busy", "other", json!({}))
            .with("idle", "other", json!({}));
    let provider = Provider::new();
    provider.install(
        "other",
        json!(LIFECYCLE),
        json!({"ok": true, "agent_id": "remote"}),
    );
    let sessions = provider.sessions(&source);

    let guard = source.locks.lock("busy").await.unwrap();
    let waiting = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        sessions.deploy("busy", Tenant::default(), None, None),
    )
    .await;
    assert!(
        waiting.is_err(),
        "the deploy must wait behind the held lock"
    );
    assert!(
        source.rebuilds.lock().unwrap().is_empty(),
        "nothing rebuilt before the lock"
    );

    // Other agents are not serialized behind it.
    sessions
        .deploy("idle", Tenant::default(), None, None)
        .await
        .unwrap();
    drop(guard);
    sessions
        .deploy("busy", Tenant::default(), None, None)
        .await
        .unwrap();
    assert_eq!(*source.rebuilds.lock().unwrap(), ["idle", "busy"]);
}
