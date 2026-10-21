//! End to end through a real agent host: a fake `claude` as Buzz's copy
//! of Claude speaks stream-json, the host runs it through the process bridge,
//! and the runtime serves it the Buzz tools as the agent. No network and no
//! Claude account.

use super::runtime::{ClaudeRuntime, Config, Run, Synced, Wake};
use super::sessions::SessionView;
use super::setup::ClaudeStatus;
use crate::agent_plugins::owner_relay::{ChannelInfo, OwnerRelay};
use crate::agent_plugins::process::{
    HostSpawner, ProcessSinks, Spawn, SpawnOptions, Spawned, Spawner,
};
use crate::buzz_mcp::app_client::AgentActions;
use crate::buzz_mcp::{BuzzEvent, ClientResult, Memory, Template, Upload};
use buzz_host::api::{HostAnswer, HostAsk, HostProtocol};
use buzz_host::credentials::FileCredentials;
use buzz_host::{AgentHost, HostConfig, RpcHandler};
use buzz_host_rpc::{
    ClientHandler, ConnectionState, HostClient, HostClientOptions, HostInfo, RpcError,
    ServerOptions, serve, transport,
};
use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const FAKE_CLAUDE: &str = include_str!("e2e/fake-claude.sh");
const CHANNEL: &str = "chan-e2e";

/// A directory of its own under the system temp directory.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("buzz-claude-e2e-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Makes the fake `claude` Buzz's copy in a data folder of its own, once.
fn fake_copy() -> &'static Path {
    static DATA: OnceLock<PathBuf> = OnceLock::new();
    DATA.get_or_init(|| {
        let data = scratch("data");
        let dir = data.join("buzz/claude-code");
        std::fs::create_dir_all(dir.join(VERSION)).unwrap();
        std::fs::write(dir.join("current"), VERSION).unwrap();
        let claude = dir.join(VERSION).join("claude");
        std::fs::write(&claude, FAKE_CLAUDE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        data
    })
}

/// The fake's release, as it reports it.
const VERSION: &str = "2.1.130";

/// Starts the host's processes with Buzz's copies under the fake's data
/// folder.
struct WithData(Spawner);

impl Spawn for WithData {
    fn spawn(
        &self,
        id: &str,
        mut options: SpawnOptions,
    ) -> BoxFuture<'static, Result<Spawned, String>> {
        options.env.insert(
            "XDG_DATA_HOME".into(),
            Some(fake_copy().to_string_lossy().into_owned()),
        );
        self.0.spawn(id, options)
    }
}

/// The GUI's side of the connection: process output only.
struct Gui(ProcessSinks);

impl ClientHandler<HostProtocol> for Gui {
    fn handle(&self, ask: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        let HostAsk::ProcessOutput { handle, output } = ask;
        let answer = self.0.deliver(handle, output);
        Box::pin(async move { answer })
    }
}

#[derive(Default)]
struct Agent {
    published: Mutex<Vec<Template>>,
}

impl AgentActions for Agent {
    fn pubkey(&self) -> String {
        "c".repeat(64)
    }
    fn owner(&self) -> String {
        "a".repeat(64)
    }
    fn name(&self) -> String {
        "Claude".into()
    }
    fn publish(&self, template: Template) -> BoxFuture<'static, ClientResult<BuzzEvent>> {
        self.published.lock().unwrap().push(template.clone());
        let event = BuzzEvent {
            id: "e".repeat(64),
            pubkey: self.pubkey(),
            kind: template.kind,
            content: template.content,
            tags: template.tags,
            ..Default::default()
        };
        Box::pin(async move { Ok(event) })
    }
    fn query(&self, _: Vec<Value>) -> BoxFuture<'static, ClientResult<Vec<BuzzEvent>>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn upload(&self, _: Vec<u8>, _: String) -> BoxFuture<'static, ClientResult<Upload>> {
        Box::pin(async { Err("no uploads".into()) })
    }
    fn remember(&self, _: String, _: String, _: u64) -> BoxFuture<'static, ClientResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

struct Relay;

impl OwnerRelay for Relay {
    fn channel(&self, _: &str) -> Option<ChannelInfo> {
        Some(ChannelInfo {
            name: Some("general".into()),
            ..Default::default()
        })
    }
    fn read(&self, _: Vec<Value>) -> BoxFuture<'static, Result<Vec<BuzzEvent>, String>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn names(&self, _: Vec<String>, _: Vec<String>) -> BoxFuture<'static, HashMap<String, String>> {
        Box::pin(async { HashMap::new() })
    }
    fn memories(&self, _: &str) -> BoxFuture<'static, Result<Vec<Memory>, String>> {
        Box::pin(async {
            Ok(vec![Memory {
                slug: "core".into(),
                body: "I am the e2e agent.".into(),
                created_at: 1,
            }])
        })
    }
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// A host embedded as the app embeds it, with the GUI's side connected.
async fn connect_host() -> (Arc<HostClient<HostProtocol>>, Spawner) {
    fake_copy();
    let state = scratch("host");
    let host = AgentHost::start(HostConfig {
        info: HostInfo {
            id: "e2e".into(),
            name: "e2e".into(),
            platform: "linux".into(),
            version: "test".into(),
        },
        state_dir: state.join("state"),
        credentials: Arc::new(FileCredentials::new(state.join("keys")).unwrap()),
        legacy_dir: None,
    })
    .unwrap();
    let (connector, listener) = transport::memory();
    tokio::spawn(serve(
        listener,
        Arc::new(RpcHandler(host)),
        ServerOptions::default(),
    ));
    let sinks = ProcessSinks::default();
    let client = Arc::new(HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(Gui(sinks.clone())),
        HostClientOptions::default(),
    ));
    sinks.follow(&client, &tokio::runtime::Handle::current());
    client
        .state()
        .wait_for(|state| matches!(state, ConnectionState::Connected { .. }))
        .await
        .unwrap();
    let spawn: Spawner = Arc::new(WithData(Arc::new(HostSpawner {
        client: client.clone(),
        sinks,
        owner: super::PROCESS_OWNER,
    })));
    (client, spawn)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mention_runs_a_claude_turn_on_the_agents_host() {
    let (_client, spawn) = connect_host().await;

    // Setup reads the version and account through the host.
    let setup = super::setup::setup(Some(spawn.clone()));
    setup.check().await;
    assert_eq!(
        setup.snapshot().status,
        Some(ClaudeStatus::Ready {
            version: VERSION.into(),
            account: Some("e2e@example.com".into()),
        })
    );

    // A mention becomes a turn; Claude answers with a final `send`.
    let sessions = scratch("sessions").join("sessions.json");
    let runtime = ClaudeRuntime::new(Some(sessions.clone()));
    let agent = Arc::new(Agent::default());
    runtime.set_relays(HashMap::from([(
        agent.pubkey(),
        Arc::new(Relay) as Arc<dyn OwnerRelay>,
    )]));
    let workspace = scratch("workspace");
    let config = Config {
        workspace: workspace.to_string_lossy().into_owned(),
        ..Config::default()
    };
    runtime.sync(vec![Synced {
        pubkey: agent.pubkey(),
        name: agent.name(),
        config: config.value(),
        spawn: spawn.clone(),
        attention: false,
    }]);
    let mention = BuzzEvent {
        id: "1".repeat(64),
        pubkey: agent.owner(),
        created_at: 100,
        kind: 9,
        content: "@Claude ping".into(),
        tags: vec![
            vec!["h".into(), CHANNEL.into()],
            vec!["p".into(), agent.pubkey()],
        ],
        sig: String::new(),
    };
    runtime
        .run(Run {
            wake: Wake::Mention {
                event: mention.clone(),
            },
            channel: Some(CHANNEL.into()),
            agent: agent.clone(),
            config: config.value(),
            spawn: spawn.clone(),
            model: None,
            effort: None,
            team: String::new(),
            working: std::sync::Arc::new(|_| {}),
            activity: None,
            current: crate::agents2::runtime::always_current(),
        })
        .await
        .unwrap();
    wait_for("the reply", || !agent.published.lock().unwrap().is_empty()).await;
    let key = format!("{CHANNEL}/{}", mention.id);
    assert_eq!(
        agent.published.lock().unwrap()[0],
        Template {
            kind: 9,
            content: "pong".into(),
            tags: vec![
                vec!["h".into(), CHANNEL.into()],
                vec![
                    "e".into(),
                    mention.id.clone(),
                    String::new(),
                    "reply".into()
                ],
            ],
        }
    );
    wait_for("the turn to settle", || {
        runtime.sessions(&agent.pubkey())
            == vec![SessionView {
                key: key.clone(),
                busy: false,
            }]
    })
    .await;
    // The session is saved under the conversation, to resume later.
    let saved: Value = serde_json::from_slice(&std::fs::read(&sessions).unwrap()).unwrap();
    assert!(saved[agent.pubkey()][&key]["id"].is_string(), "{saved}");

    // A file is read on the host from the agent's workspace.
    std::fs::write(workspace.join("report.md"), "from a file").unwrap();
    let follow_up = BuzzEvent {
        id: "2".repeat(64),
        created_at: 120,
        content: "READ:report.md please".into(),
        tags: vec![
            vec!["h".into(), CHANNEL.into()],
            vec![
                "e".into(),
                mention.id.clone(),
                String::new(),
                "reply".into(),
            ],
        ],
        ..mention.clone()
    };
    runtime
        .run(Run {
            wake: Wake::Mention { event: follow_up },
            channel: Some(CHANNEL.into()),
            agent: agent.clone(),
            config: config.value(),
            spawn,
            model: None,
            effort: None,
            team: String::new(),
            working: std::sync::Arc::new(|_| {}),
            activity: None,
            current: crate::agents2::runtime::always_current(),
        })
        .await
        .unwrap();
    wait_for("the file", || agent.published.lock().unwrap().len() == 2).await;
    assert_eq!(agent.published.lock().unwrap()[1].content, "from a file");
    runtime.dispose();
}

/// Against the TLS test relay with a throwaway agent owned by alice: the
/// owner reads its core memory back, and a mention's turn publishes its
/// reply through the Buzz tools as the agent. Skipped unless
/// `BUZZ_TEST_RELAY` names the test relay's state directory.
#[tokio::test(flavor = "multi_thread")]
async fn tools_publish_to_the_test_relay_as_a_throwaway_agent() {
    use crate::agents2::AgentHandle;
    use crate::agents2::runtime::HostLink;
    use buzz_host::api::{AppAgentRequest, AppAgentResponse, Request, Response};
    use buzz_identity::{Identity, Purpose};
    use buzz_relay_client::{RelayHttp, RelayOrigin};
    use serde_json::json;

    let Some(dir) = std::env::var_os("BUZZ_TEST_RELAY").map(PathBuf::from) else {
        eprintln!("BUZZ_TEST_RELAY not set; skipping");
        return;
    };
    let read = |path: PathBuf| -> Value {
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    };
    let identities = read(dir.join("identities.json"));
    let tls = read(dir.join("tls/relay.json"));
    // The bundle trusts the test relay's CA as well as the usual roots.
    // SAFETY: before this test makes any TLS connection; no other test in
    // this binary connects over TLS.
    unsafe { std::env::set_var("SSL_CERT_FILE", tls["ca_bundle"].as_str().unwrap()) };
    let alice = Arc::new(
        Identity::from_nsec(identities["identities"]["alice"]["nsec"].as_str().unwrap()).unwrap(),
    );
    let relay = tls["relay_ws_url"].as_str().unwrap().to_owned();
    let general = tls["channels"]["general"].as_str().unwrap().to_owned();
    let (client, spawn) = connect_host().await;
    let ask = |request: AppAgentRequest| {
        let client = client.clone();
        async move {
            match client.request(Request::AppAgent(request)).await {
                Ok(Response::AppAgent(response)) => response,
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    let AppAgentResponse::Prepared { pubkey } = ask(AppAgentRequest::CreatePrepare {
        request_id: "claude-code-relay".into(),
        destination: relay.clone(),
        owner: alice.pubkey_hex(),
        agent_type: format!("{}/{}", super::PLUGIN, super::TYPE_ID),
        name: "Claude Test".into(),
    })
    .await
    else {
        panic!("expected prepared");
    };
    ask(AppAgentRequest::CreateCommit {
        request_id: "claude-code-relay".into(),
        auth: alice.attest_agent(&pubkey).unwrap(),
    })
    .await;
    let http = RelayHttp::new(RelayOrigin::parse(&relay).unwrap(), alice.clone());
    let now = || chrono::Utc::now().timestamp() as u64;
    let post = |kind: u16, tags: Vec<Vec<String>>, content: String| {
        let (alice, http) = (alice.clone(), http.clone());
        async move {
            let unsigned = alice
                .template(Purpose::Content, kind, now(), tags, content)
                .unwrap();
            let signed = alice.sign(Purpose::Content, unsigned).unwrap();
            let receipt = http
                .post("/events", serde_json::to_string(&signed).unwrap())
                .await
                .unwrap();
            let receipt: Value = serde_json::from_slice(&receipt).unwrap();
            assert_eq!(receipt["accepted"], true, "kind {kind} refused: {receipt}");
            signed
        }
    };
    let membership = vec![
        vec!["h".to_string(), general.clone()],
        vec!["p".to_string(), pubkey.clone()],
    ];
    post(9000, membership.clone(), String::new()).await;

    let handle = AgentHandle {
        pubkey: pubkey.clone(),
        name: "Claude Test".into(),
        owner: alice.pubkey_hex(),
        host: HostLink {
            client: client.clone(),
            runtime: tokio::runtime::Handle::current(),
        },
        wrote: Default::default(),
        attention: crate::agents2::runtime::AgentAttention::detached(&pubkey),
    };
    // Core memory, written as the agent, is read back with the owner's key.
    handle
        .remember("core", "I am the relay test agent.", 0)
        .await
        .unwrap();
    let owner = Arc::new(crate::agent_plugins::owner_relay::AppRelay {
        http: http.clone(),
        identity: alice.clone(),
        snapshot: None,
        session: None,
    });
    let memories = owner.memories(&pubkey).await.unwrap();
    assert!(
        memories
            .iter()
            .any(|memory| memory.slug == "core" && memory.body == "I am the relay test agent."),
        "{memories:?}"
    );

    // alice mentions it; its turn replies in her thread as the agent.
    let mention = post(
        9,
        vec![
            vec!["h".into(), general.clone()],
            vec!["p".into(), pubkey.clone()],
        ],
        format!("@Claude Test ping {}", now()),
    )
    .await;
    let event: BuzzEvent = serde_json::from_value(serde_json::to_value(&mention).unwrap()).unwrap();
    let runtime = ClaudeRuntime::new(Some(scratch("relay-sessions").join("sessions.json")));
    runtime.set_relays(HashMap::from([(
        pubkey.clone(),
        owner as Arc<dyn OwnerRelay>,
    )]));
    let config = Config {
        workspace: scratch("relay-workspace").to_string_lossy().into_owned(),
        ..Config::default()
    };
    runtime
        .run(Run {
            wake: Wake::Mention {
                event: event.clone(),
            },
            channel: Some(general.clone()),
            agent: Arc::new(handle),
            config: config.value(),
            spawn,
            model: None,
            effort: None,
            team: String::new(),
            working: std::sync::Arc::new(|_| {}),
            activity: None,
            current: crate::agents2::runtime::always_current(),
        })
        .await
        .unwrap();
    let filter = json!({ "kinds": [9], "authors": [pubkey], "#e": [event.id], "limit": 5 });
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let replies = http
            .query(&[filter.as_object().unwrap().clone()])
            .await
            .unwrap();
        if replies.iter().any(|reply| reply.content == "pong") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the agent's reply was not published"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    runtime.dispose();

    // Clean up: alice removes it from #general and its key is deleted.
    post(9001, membership, String::new()).await;
    ask(AppAgentRequest::Delete {
        pubkey: pubkey.clone(),
    })
    .await;
    ask(AppAgentRequest::Forget { pubkey }).await;
}
