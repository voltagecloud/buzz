//! Remote workspace requests through a host, against a stub
//! `buzz-backend-kubernetes` that records what it is asked.

use buzz_host::api::*;
use buzz_host::credentials::FileCredentials;
use buzz_host::{AgentHost, HostConfig, RpcHandler};
use buzz_host_rpc::{
    ClientHandler, ConnectionState, HostClient, HostClientOptions, HostInfo, RpcError,
    ServerOptions, serve, transport,
};
use buzz_identity::Identity;
use buzz_sandbox_host::Sandboxes;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const RELAY: &str = "wss://relay.example";
const CHANNEL: &str = "f9ee507e-d03c-4570-baa6-02951c38e1cd";

/// A stub provider in its own directory.
struct Provider {
    dir: tempfile::TempDir,
}

impl Provider {
    /// Answers `info` with session support, and every other request with
    /// `response` after `delay` seconds. Requests are appended to
    /// `requests`; one that starts while another runs leaves `overlap`.
    fn new(response: Value, delay: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let info = json!({"ok": true, "name": "kubernetes", "version": "1",
            "protocol_version": 1, "description": "Stub", "config_schema": {},
            "ops": ["info", "deploy", "stop", "recover"]});
        let d = dir.path().display();
        let script = format!(
            "#!/bin/sh\nIFS= read -r request\ncase \"$request\" in\n\
             *'\"op\":\"info\"'*) printf '%s\\n' '{info}' ;;\n\
             *) [ -e '{d}/busy' ] && : > '{d}/overlap'\n: > '{d}/busy'\n\
             printf '%s\\n' \"$request\" >> '{d}/requests'\nsleep {delay}\n\
             rm -f '{d}/busy'\nprintf '%s\\n' '{response}' ;;\nesac\n"
        );
        let path = dir.path().join("buzz-backend-kubernetes");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("requests"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn overlapped(&self) -> bool {
        self.dir.path().join("overlap").exists()
    }

    fn busy(&self) -> PathBuf {
        self.dir.path().join("busy")
    }
}

fn host(state: &Path, provider: &Provider) -> Arc<AgentHost> {
    let sandboxes = Sandboxes::new(&state.join("state"))
        .with_provider_dirs(vec![provider.dir.path().to_path_buf()]);
    AgentHost::start(HostConfig {
        info: HostInfo {
            id: "devbox".into(),
            name: "Devbox".into(),
            platform: "linux".into(),
            version: "test".into(),
        },
        state_dir: state.join("state"),
        credentials: Arc::new(FileCredentials::new(state.join("keys")).unwrap()),
        legacy_dir: None,
        sandbox: Some(Arc::new(sandboxes)),
    })
    .unwrap()
}

async fn app_agent(host: &AgentHost, request: AppAgentRequest) -> AppAgentResponse {
    match host.request(Request::AppAgent(request)).await.unwrap() {
        Response::AppAgent(response) => response,
        other => panic!("expected an app agent response, got {other:?}"),
    }
}

/// Makes an agent; returns its pubkey and its owner.
async fn agent(host: &AgentHost) -> (String, Identity) {
    let owner = Identity::ephemeral();
    let prepare = AppAgentRequest::CreatePrepare {
        request_id: "create".into(),
        destination: RELAY.into(),
        owner: owner.pubkey_hex(),
        agent_type: "voltage.remote-workspace/kubernetes".into(),
        name: "Ada".into(),
    };
    let AppAgentResponse::Prepared { pubkey } = app_agent(host, prepare).await else {
        panic!("expected prepared");
    };
    let commit = AppAgentRequest::CreateCommit {
        request_id: "create".into(),
        auth: owner.attest_agent(&pubkey).unwrap(),
    };
    app_agent(host, commit).await;
    (pubkey, owner)
}

fn session(pubkey: &str, owner: &Identity, root: char) -> SandboxSession {
    SandboxSession {
        pubkey: pubkey.into(),
        tenant: SandboxTenant {
            relay: RELAY.into(),
            signer: owner.pubkey_hex(),
        },
        scope: SandboxScope {
            channel_id: CHANNEL.into(),
            thread_root: root.to_string().repeat(64),
        },
        spec: LaunchSpec {
            provider: "kubernetes".into(),
            provider_config_json: json!({"namespace": "agents", "sandbox": true}).to_string(),
            env: [
                ("BUZZ_PRIVATE_KEY".into(), "nsec1fromthegui".into()),
                ("API_TOKEN".into(), "user".into()),
            ]
            .into(),
            launch: Launch {
                command: "claude-agent-acp".into(),
                env: [("BUZZ_RELAY_URL".into(), "wss://elsewhere".into())].into(),
                policy_env: [("BUZZ_AUTH_TAG".into(), "forged".into())].into(),
                ..Default::default()
            },
            name: "Ada".into(),
        },
    }
}

fn start(session: SandboxSession) -> SandboxRequest {
    SandboxRequest::Start {
        session,
        replay_floor: Some(1_700_000_000),
    }
}

async fn sandbox(host: &AgentHost, request: SandboxRequest) -> SandboxResponse {
    match host.request(Request::Sandbox(request)).await.unwrap() {
        Response::Sandbox(response) => response,
        other => panic!("expected a sandbox response, got {other:?}"),
    }
}

fn deployed() -> Value {
    json!({"ok": true, "agent_id": "agent-1"})
}

#[tokio::test(flavor = "multi_thread")]
async fn a_start_carries_the_bound_scope_and_custody_identity() {
    let state = tempfile::tempdir().unwrap();
    let provider = Provider::new(deployed(), "0");
    let host = host(state.path(), &provider);
    let (pubkey, owner) = agent(&host).await;
    let response = sandbox(&host, start(session(&pubkey, &owner, 'a'))).await;
    assert!(matches!(response, SandboxResponse::Started), "{response:?}");

    let [request] = provider.requests().try_into().unwrap();
    assert_eq!(request["op"], "deploy");
    let config = &request["provider_config"];
    assert_eq!(
        config["sandbox"],
        json!({"channel_id": CHANNEL, "thread_root": "a".repeat(64)})
    );
    assert_eq!(
        config["identity_policy"],
        json!({"agent_pubkey": pubkey, "owner_pubkey": owner.pubkey_hex()})
    );
    assert_eq!(config["namespace"], "agents");
    let agent = &request["agent"];
    let nsec = agent["private_key_nsec"].as_str().unwrap();
    assert_eq!(Identity::from_nsec(nsec).unwrap().pubkey_hex(), pubkey);
    assert_eq!(agent["relay_url"], RELAY);
    let auth: Vec<String> = serde_json::from_str(agent["auth_tag"].as_str().unwrap()).unwrap();
    assert_eq!(auth[1], owner.pubkey_hex());
    let launch = &agent["launch"];
    assert_eq!(launch["owner_pubkey"], owner.pubkey_hex());
    assert_eq!(launch["command"], "claude-agent-acp");
    assert_eq!(launch["env"], json!({"API_TOKEN": "user"}));
    assert_eq!(agent["env_vars"], json!({"API_TOKEN": "user"}));
    assert_eq!(
        launch["policy_env"],
        json!({"BUZZ_ACP_REPLAY_FLOOR": "1700000000"})
    );
    let text = request.to_string();
    assert!(
        !text.contains("fromthegui") && !text.contains("elsewhere") && !text.contains("forged")
    );

    // A config that names the identity policy itself is refused unsent.
    let mut forged = session(&pubkey, &owner, 'a');
    forged.spec.provider_config_json =
        json!({"sandbox": true, "identity_policy": {"owner_pubkey": "e".repeat(64)}}).to_string();
    let refused = sandbox(&host, start(forged)).await;
    assert!(
        matches!(refused, SandboxResponse::Failed { .. }),
        "{refused:?}"
    );
    assert_eq!(provider.requests().len(), 1);
}

struct NoAsks;

impl ClientHandler<HostProtocol> for NoAsks {
    fn handle(&self, _: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        Box::pin(async { Err(RpcError::Failed("unexpected ask".into())) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_refusal_arrives_with_its_code() {
    let state = tempfile::tempdir().unwrap();
    let ended = json!({"ok": false, "error": "The session ended", "code": "session_ended"});
    let provider = Provider::new(ended, "0");
    let host = host(state.path(), &provider);
    let (pubkey, owner) = agent(&host).await;
    let (connector, listener) = transport::memory();
    tokio::spawn(serve(
        listener,
        Arc::new(RpcHandler(host)),
        ServerOptions::default(),
    ));
    let client = HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(NoAsks),
        HostClientOptions::default(),
    );
    client
        .state()
        .wait_for(|s| matches!(s, ConnectionState::Connected { .. }))
        .await
        .unwrap();
    let request = Request::Sandbox(start(session(&pubkey, &owner, 'a')));
    match client.request(request).await.unwrap() {
        Response::Sandbox(SandboxResponse::Failed { message, code }) => {
            assert_eq!(code.as_deref(), Some("session_ended"));
            assert!(message.contains("The session ended"), "{message}");
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn starts_for_one_agent_never_overlap() {
    let state = tempfile::tempdir().unwrap();
    let provider = Provider::new(deployed(), "0.5");
    let host = host(state.path(), &provider);
    let (pubkey, owner) = agent(&host).await;
    let (first, second) = tokio::join!(
        sandbox(&host, start(session(&pubkey, &owner, 'a'))),
        sandbox(&host, start(session(&pubkey, &owner, 'b'))),
    );
    assert!(matches!(first, SandboxResponse::Started), "{first:?}");
    assert!(matches!(second, SandboxResponse::Started), "{second:?}");
    assert_eq!(provider.requests().len(), 2);
    assert!(!provider.overlapped());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_deleted_while_a_start_waits_is_not_started() {
    let state = tempfile::tempdir().unwrap();
    let provider = Provider::new(deployed(), "1");
    let host = host(state.path(), &provider);
    let (pubkey, owner) = agent(&host).await;
    let first = tokio::spawn({
        let (host, session) = (host.clone(), session(&pubkey, &owner, 'a'));
        async move { sandbox(&host, start(session)).await }
    });
    while !provider.busy().exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Its key is read before it waits for the first start's lock.
    let second = tokio::spawn({
        let (host, session) = (host.clone(), session(&pubkey, &owner, 'b'));
        async move { sandbox(&host, start(session)).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    app_agent(
        &host,
        AppAgentRequest::Delete {
            pubkey: pubkey.clone(),
        },
    )
    .await;
    assert!(matches!(first.await.unwrap(), SandboxResponse::Started));
    let refused = second.await.unwrap();
    assert!(
        matches!(&refused, SandboxResponse::Failed { message, .. }
            if message == "No such agent on this device"),
        "{refused:?}"
    );
    assert_eq!(provider.requests().len(), 1, "only the first start ran");
}
