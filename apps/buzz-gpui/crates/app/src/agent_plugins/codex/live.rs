//! Opt-in checks against the installed Codex CLI and its existing sign-in,
//! through a real agent host (buzz-app's `live.mjs`). They send no Buzz
//! messages. Run with
//! `cargo test -p buzz-gpui codex::live -- --ignored`; set
//! `BUZZ_CODEX_TEST_MODEL` to pick the model for the turn.

use super::PROCESS_OWNER;
use super::runtime::{CodexRuntime, Run, Storage, Wake};
use super::tabs::catalog;
use crate::agent_plugins::owner_relay::{ChannelInfo, OwnerRelay};
use crate::agent_plugins::process::{HostSpawner, ProcessSinks, Spawner};
use crate::buzz_mcp::app_client::{AgentActions, Signal};
use crate::buzz_mcp::{BuzzEvent, ClientResult, Memory, Template, Upload};
use buzz_host::api::{HostAnswer, HostAsk, HostProtocol};
use buzz_host::credentials::FileCredentials;
use buzz_host::{AgentHost, HostConfig, RpcHandler};
use buzz_host_rpc::{
    ClientHandler, ConnectionState, HostClient, HostClientOptions, HostInfo, RpcError,
    ServerOptions, serve, transport,
};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("buzz-codex-live-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Gui(ProcessSinks);

impl ClientHandler<HostProtocol> for Gui {
    fn handle(&self, ask: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        let HostAsk::ProcessOutput { handle, output } = ask;
        let answer = self.0.deliver(handle, output);
        Box::pin(async move { answer })
    }
}

/// A host embedded as the app embeds it, running Codex's processes.
async fn host() -> Spawner {
    let state = scratch("host");
    let host = AgentHost::start(HostConfig {
        info: HostInfo {
            id: "live".into(),
            name: "live".into(),
            platform: "linux".into(),
            version: "test".into(),
        },
        state_dir: state.join("state"),
        credentials: Arc::new(FileCredentials::new(state.join("keys")).unwrap()),
        legacy_dir: None,
        sandbox: None,
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
    Arc::new(HostSpawner {
        client,
        sinks,
        owner: PROCESS_OWNER,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the installed Codex CLI with its sign-in"]
async fn reads_the_signed_in_account_and_models() {
    let catalog = catalog(host().await, Signal::default()).await.unwrap();
    assert!(!catalog.account.is_empty());
    assert!(catalog.models.iter().any(|model| model.is_default));
    println!(
        "account type ok; {} models, default {}",
        catalog.models.len(),
        catalog
            .models
            .iter()
            .find(|model| model.is_default)
            .map(|model| model.model.as_str())
            .unwrap_or_default()
    );
}

#[derive(Default)]
struct Agent {
    published: Mutex<Vec<Template>>,
}

impl AgentActions for Agent {
    fn pubkey(&self) -> String {
        "b".repeat(64)
    }
    fn owner(&self) -> String {
        "a".repeat(64)
    }
    fn name(&self) -> String {
        "Codex Live".into()
    }
    fn publish(&self, template: Template) -> BoxFuture<'static, ClientResult<BuzzEvent>> {
        self.published.lock().unwrap().push(template.clone());
        let event = BuzzEvent {
            id: "d".repeat(64),
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
            name: Some("live".into()),
            dm: false,
            members: vec![],
        })
    }
    fn read(&self, _: Vec<Value>) -> BoxFuture<'static, Result<Vec<BuzzEvent>, String>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn names(&self, _: Vec<String>, _: Vec<String>) -> BoxFuture<'static, HashMap<String, String>> {
        Box::pin(async { HashMap::new() })
    }
    fn memories(&self, _: &str) -> BoxFuture<'static, Result<Vec<Memory>, String>> {
        Box::pin(async { Ok(vec![]) })
    }
}

#[derive(Default)]
struct Memoryless(Mutex<HashMap<String, Value>>);

impl Storage for Memoryless {
    fn get(&self, key: &str) -> Option<Value> {
        self.0.lock().unwrap().get(key).cloned()
    }
    fn set(&self, key: &str, value: Value) -> Result<(), String> {
        self.0.lock().unwrap().insert(key.into(), value);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs a real Codex turn with its sign-in"]
async fn answers_a_mention_through_the_buzz_send_tool() {
    let spawn = host().await;
    let runtime = CodexRuntime::with_storage(Arc::new(Memoryless::default()));
    let scope = "https://relay.live:owner";
    let agent = Arc::new(Agent::default());
    runtime.set_relays(HashMap::from([(
        agent.pubkey(),
        (scope.into(), Arc::new(Relay) as Arc<dyn OwnerRelay>),
    )]));
    runtime.sync(&[agent.pubkey()]);
    let workspace = scratch("workspace");
    let model = std::env::var("BUZZ_CODEX_TEST_MODEL").unwrap_or_else(|_| "gpt-6-luna".into());
    let event = BuzzEvent {
        id: "1".repeat(64),
        pubkey: agent.owner(),
        created_at: 10,
        kind: 9,
        tags: vec![vec!["h".into(), "live".into()]],
        content: "@Codex Live Reply with exactly the word PONG. Do not run any commands.".into(),
        sig: String::new(),
    };
    runtime
        .run(Run {
            wake: Wake::Mention { event },
            channel: Some("live".into()),
            agent: agent.clone(),
            config: json!({
                "workspace": workspace.to_string_lossy(),
                "model": model,
                "effort": "low",
            }),
            spawn,
            model: None,
            effort: None,
            team: String::new(),
            working: std::sync::Arc::new(|_| {}),
            activity: None,
        })
        .await
        .unwrap();
    let pubkey = agent.pubkey();
    tokio::time::timeout(Duration::from_secs(240), async {
        while runtime.sessions(&pubkey).first().is_none_or(|view| {
            !matches!(
                view.status.as_str(),
                "Idle" | "Needs attention" | "Interrupted"
            )
        }) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("the turn did not finish");
    let published: Vec<String> = agent
        .published
        .lock()
        .unwrap()
        .iter()
        .map(|template| template.content.clone())
        .collect();
    println!(
        "status {:?}; posted {published:?}",
        runtime.sessions(&pubkey)
    );
    assert_eq!(runtime.sessions(&pubkey)[0].status, "Idle");
    assert!(published.iter().any(|content| content.contains("PONG")));
    runtime.dispose();
    let _ = std::fs::remove_dir_all(workspace);
}
