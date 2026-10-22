use super::config::Config;
use super::settings::{Draft, parse, relay_fields};
use super::*;
use crate::agents2::attention::{Channels, EventData, EventWatch, TimerWatch};
use crate::agents2::runtime::{AgentAttention, AgentHandle, CancelSource, Working, always_current};
use buzz_host::api::{HostAnswer, HostAsk, HostEvent, HostProtocol, Request, Response};
use buzz_host_rpc::{
    ClientHandler, ConnectionState, EventSource, HostClient, HostClientOptions, HostHandler,
    HostInfo, Peer, RpcError, ServerOptions, serve, transport,
};
use core::prelude::v1::test;
use futures::future::BoxFuture;
use serde_json::json;
use std::sync::Mutex;
use tokio::sync::broadcast;

const OWNER: &str = "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11";
const CHANNEL: &str = "f9ee507e-d03c-4570-baa6-02951c38e1cd";
const RELAY: &str = "wss://relay.example";

/// A host that starts every session and keeps what it was sent.
struct Recording {
    events: broadcast::Sender<(u64, HostEvent)>,
    sent: Arc<Mutex<Vec<SandboxRequest>>>,
}

impl HostHandler<HostProtocol> for Recording {
    fn info(&self) -> HostInfo {
        HostInfo {
            id: "devbox".into(),
            name: "Devbox".into(),
            platform: "linux".into(),
            version: "test".into(),
        }
    }

    fn events(&self) -> EventSource<HostProtocol> {
        self.events.subscribe()
    }

    fn handle(
        &self,
        _: Peer<HostProtocol>,
        request: Request,
    ) -> BoxFuture<'static, Result<Response, RpcError>> {
        let sent = self.sent.clone();
        Box::pin(async move {
            match request {
                Request::Sandbox(request) => {
                    sent.lock().unwrap().push(request);
                    Ok(Response::Sandbox(SandboxResponse::Started))
                }
                _ => Err(RpcError::Failed("unexpected request".into())),
            }
        })
    }
}

struct NoAsks;

impl ClientHandler<HostProtocol> for NoAsks {
    fn handle(&self, _: HostAsk) -> BoxFuture<'static, Result<HostAnswer, RpcError>> {
        Box::pin(async { Err(RpcError::Failed("unexpected ask".into())) })
    }
}

/// A link to a recording host, and what it was sent.
async fn host() -> (HostLink, Arc<Mutex<Vec<SandboxRequest>>>) {
    let (connector, listener) = transport::memory();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let host = Recording {
        events: broadcast::channel(1).0,
        sent: sent.clone(),
    };
    tokio::spawn(serve(listener, Arc::new(host), ServerOptions::default()));
    let client = Arc::new(HostClient::<HostProtocol>::spawn(
        &tokio::runtime::Handle::current(),
        Arc::new(connector),
        Arc::new(NoAsks),
        HostClientOptions::default(),
    ));
    client
        .state()
        .wait_for(|state| matches!(state, ConnectionState::Connected { .. }))
        .await
        .unwrap();
    let link = HostLink {
        client,
        runtime: tokio::runtime::Handle::current(),
    };
    (link, sent)
}

fn config() -> serde_json::Value {
    json!({
        "provider_config": {"relay_url": RELAY, "manager_pubkey": MANAGER},
        "env": {"BUZZ_CHECKPOINT_PREFIX": "tee8z/"},
        "model": "opus",
    })
}

/// A reply in thread `root`, by the owner.
fn mention(root: &str) -> Arc<EventData> {
    Arc::new(EventData {
        id: "e".repeat(64),
        pubkey: OWNER.into(),
        created_at: 1_700_000_123,
        kind: 9,
        content: "hi".into(),
        tags: vec![
            vec!["h".into(), CHANNEL.into()],
            vec!["e".into(), root.into(), String::new(), "reply".into()],
        ],
    })
}

fn delivery(trigger: Trigger, host: HostLink) -> Delivery {
    let pubkey = "b".repeat(64);
    Delivery {
        trigger,
        channel: Some(CHANNEL.into()),
        agent: AgentHandle {
            pubkey: pubkey.clone(),
            name: "Ada".into(),
            owner: OWNER.into(),
            host,
            wrote: Default::default(),
            attention: AgentAttention::detached(&pubkey),
        },
        config: config(),
        model: None,
        effort: None,
        team: None,
        working: Working::none(tokio::runtime::Handle::current()),
        cancel: CancelSource::new().1,
        current: always_current(),
    }
}

#[tokio::test]
async fn a_mention_starts_its_threads_session() {
    let (link, sent) = host().await;
    let root = "c".repeat(64);
    let started = deliver(
        delivery(
            Trigger::Mention {
                event: mention(&root),
            },
            link,
        ),
        RELAY.into(),
    )
    .await
    .unwrap();
    assert!(matches!(started, Some((_, Ok(())))), "{started:?}");
    let sent: [SandboxRequest; 1] = sent.lock().unwrap().clone().try_into().unwrap();
    let [
        SandboxRequest::Start {
            session,
            replay_floor,
        },
    ] = sent
    else {
        panic!("expected one Start, got {sent:?}");
    };
    assert_eq!(replay_floor, Some(1_700_000_123));
    assert_eq!(session.pubkey, "b".repeat(64));
    assert_eq!(session.scope.channel_id, CHANNEL);
    assert_eq!(session.scope.thread_root, root);
    assert_eq!(session.tenant.relay, RELAY);
    assert_eq!(session.tenant.signer, OWNER);
    let config: serde_json::Value =
        serde_json::from_str(&session.spec.provider_config_json).unwrap();
    assert_eq!(config["sandbox"], true, "always a thread Sandbox");
    assert_eq!(config["relay_url"], RELAY);
    assert_eq!(config["manager_pubkey"], MANAGER);
    assert_eq!(session.spec.env["BUZZ_CHECKPOINT_PREFIX"], "tee8z/");
    assert_eq!(session.spec.launch.command, "claude-agent-acp");
    assert_eq!(session.spec.launch.policy_env["ANTHROPIC_MODEL"], "opus");
}

#[tokio::test]
async fn watches_timers_and_strangers_start_nothing() {
    let (link, sent) = host().await;
    let watch = Trigger::Watch {
        event: mention(&"c".repeat(64)),
        slug: "watch".into(),
        watch: EventWatch {
            interest_id: "i".into(),
            name: None,
            enabled: true,
            since: 0,
            channels: Channels::ALL,
            kinds: vec![9],
            tags: None,
            filter: None,
            classifier: None,
        },
        interest: None,
        classifier: None,
    };
    let timer = Trigger::Timer {
        slug: "timer".into(),
        timer: TimerWatch {
            interest_id: "i".into(),
            prompt: "Check in".into(),
            enabled: true,
            interval_secs: 60,
            armed_at: 0,
            max_occurrences: None,
            expires_at: None,
        },
        interest: None,
        spent: false,
    };
    let mut stranger = (*mention(&"c".repeat(64))).clone();
    stranger.pubkey = "d".repeat(64);
    let stranger = Trigger::Mention {
        event: Arc::new(stranger),
    };
    for trigger in [watch, timer, stranger] {
        let started = deliver(delivery(trigger, link.clone()), RELAY.into()).await;
        assert!(matches!(started, Ok(None)), "{started:?}");
    }
    assert!(sent.lock().unwrap().is_empty());
}

#[test]
fn settings_refuse_a_bad_environment() {
    let saved = Config::read(&config());
    let parsed = |env: &str| {
        let draft = Draft {
            runtime: "codex",
            env,
            ..relay_draft(RELAY, MANAGER)
        };
        parse(&saved, draft)
    };
    let config = parsed(r#"{"BUZZ_CHECKPOINT_PREFIX": "tee8z/"}"#).unwrap();
    assert_eq!(config.provider_config["sandbox"], true);
    assert_eq!(config.runtime, "codex");
    for env in ["{", "[]", r#"{"BAD-NAME": "x"}"#, r#"{"COUNT": 1}"#] {
        assert!(parsed(env).is_err(), "{env}");
    }
}

const MANAGER: &str = "5be7aa6bbcf1e8a4b4b4bd0b2a2a0e0c3fa4a4b7f5a1d1cb6d1d2e7f9c0a1b2c";

/// A relay draft for `relay` and `manager`.
fn relay_draft<'a>(relay: &'a str, manager: &'a str) -> Draft<'a> {
    Draft {
        runtime: "claude",
        relay_url: relay,
        manager_pubkey: manager,
        env: "{}",
        ..Default::default()
    }
}

#[test]
fn settings_take_the_relay_provider_by_its_relay_and_manager_only() {
    let saved = Config::read(&config());
    let config = parse(&saved, relay_draft(" wss://relay.example ", MANAGER)).unwrap();
    assert_eq!(config.provider, "relay");
    assert_eq!(
        config.provider_config,
        json!({"relay_url": RELAY, "manager_pubkey": MANAGER, "sandbox": true})
    );
    let (upper, long) = (MANAGER.to_uppercase(), format!("{MANAGER}0"));
    for (relay, manager) in [
        ("https://relay.example", MANAGER),
        ("wss://", MANAGER),
        ("", MANAGER),
        (RELAY, ""),
        (RELAY, upper.as_str()),
        (RELAY, &MANAGER[1..]),
        (RELAY, long.as_str()),
    ] {
        assert!(
            parse(&saved, relay_draft(relay, manager)).is_err(),
            "{relay} {manager}"
        );
    }
}

#[test]
fn validation_follows_each_providers_schema() {
    let relay = |provider_config: serde_json::Value| {
        Config::read(&json!({"provider": "relay", "provider_config": provider_config}))
    };
    let base = json!({"relay_url": RELAY, "manager_pubkey": MANAGER});
    assert!(relay(base.clone()).validate().is_ok());
    let mut timed = base.clone();
    timed["timeout_seconds"] = json!(900);
    assert!(relay(timed.clone()).validate().is_ok());
    for (key, value) in [
        ("timeout_seconds", json!(901)),
        ("timeout_seconds", json!(0)),
        ("namespace", json!("agents")),
        ("image", json!("buzz-agent")),
    ] {
        let mut config = base.clone();
        config[key] = value;
        assert!(relay(config).validate().is_err(), "{key}");
    }
}

#[test]
fn the_relay_field_starts_as_the_agents_community() {
    assert_eq!(
        relay_fields(&Config::default(), "wss://community.example"),
        ("wss://community.example".into(), String::new())
    );
    let saved = Config::read(&json!({
        "provider": "relay",
        "provider_config": {"relay_url": RELAY, "manager_pubkey": MANAGER},
    }));
    assert_eq!(
        relay_fields(&saved, "wss://community.example"),
        (RELAY.into(), MANAGER.into())
    );
}

#[tokio::test]
async fn a_relay_mention_starts_its_session_through_the_host() {
    let (link, sent) = host().await;
    let mut delivery = delivery(
        Trigger::Mention {
            event: mention(&"c".repeat(64)),
        },
        link,
    );
    delivery.config = json!({
        "provider": "relay",
        "provider_config": {"relay_url": RELAY, "manager_pubkey": MANAGER},
    });
    let started = deliver(delivery, RELAY.into()).await.unwrap();
    assert!(matches!(started, Some((_, Ok(())))), "{started:?}");
    let [SandboxRequest::Start { session, .. }] = &sent.lock().unwrap()[..] else {
        panic!("expected one Start");
    };
    assert_eq!(session.spec.provider, "relay");
    let config: serde_json::Value =
        serde_json::from_str(&session.spec.provider_config_json).unwrap();
    assert_eq!(
        config,
        json!({"relay_url": RELAY, "manager_pubkey": MANAGER, "sandbox": true})
    );
}

#[test]
fn the_launch_follows_the_desktops_policy() {
    let mut config = Config::read(&config());
    config.env.insert("ANTHROPIC_MODEL".into(), "haiku".into());
    config
        .env
        .insert("BUZZ_ACP_SESSION_POLICY".into(), "channel".into());
    config.instructions = "Be brief.".into();
    let spec = config.spec("Ada\u{0}", Some("Team Blue"));
    let policy = &spec.launch.policy_env;
    for (key, value) in [
        ("BUZZ_ACP_RELAY_OBSERVER", "true"),
        ("BUZZ_ACP_LAZY_POOL", "true"),
        ("BUZZ_ACP_AGENTS", "1"),
        ("BUZZ_ACP_SESSION_POLICY", "thread"),
        ("BUZZ_ACP_SYSTEM_PROMPT", "Be brief."),
        ("ANTHROPIC_MODEL", "opus"),
        ("BUZZ_ACP_SESSION_TITLE", "Ada"),
        ("BUZZ_ACP_DISPLAY_NAME", "Ada"),
        ("BUZZ_ACP_TEAM_INSTRUCTIONS", "Team Blue"),
    ] {
        assert_eq!(policy.get(key).map(String::as_str), Some(value), "{key}");
    }
    // The user's keys would win over policy_env, so these are dropped.
    assert_eq!(
        spec.env.keys().collect::<Vec<_>>(),
        vec!["BUZZ_CHECKPOINT_PREFIX"]
    );

    config.runtime = "goose".into();
    let goose = config.spec("Ada", None);
    assert_eq!(goose.launch.args, vec!["acp"]);
    assert_eq!(goose.launch.policy_env["GOOSE_MODE"], "auto");
    assert_eq!(goose.launch.policy_env["BUZZ_ACP_MODEL"], "opus");
    assert!(goose.env.contains_key("ANTHROPIC_MODEL"));

    // The Buzz agent reads its startup model, such as an OpenRouter model ID, itself.
    config.runtime = "buzz-agent".into();
    config.model = "qwen/qwen3-coder".into();
    let buzz = config.spec("Ada", None);
    assert_eq!(
        buzz.launch.policy_env["BUZZ_AGENT_MODEL"],
        "qwen/qwen3-coder"
    );
    assert!(!buzz.launch.policy_env.contains_key("BUZZ_ACP_MODEL"));
}

/// Changing an agent's type to or from a Remote workspace keeps its
/// instructions, as the harness types do; a local workspace stays behind.
#[test]
fn instructions_carry_across_a_type_change() {
    let carry = buzz_host::api::Carry {
        instructions: Some("You are Bifrost.".into()),
        workspace: Some("~/src".into()),
    };
    let adopted = RemoteWorkspace.adopt(&carry, Config::default().value());
    assert_eq!(Config::read(&adopted).instructions, "You are Bifrost.");
    let kept = RemoteWorkspace.carry(&adopted);
    assert_eq!(kept.instructions.as_deref(), Some("You are Bifrost."));
    assert_eq!(kept.workspace, None);
    assert_eq!(
        RemoteWorkspace
            .carry(&Config::default().value())
            .instructions,
        None
    );
}

/// Every agent runs through the Sandbox manager: a new one starts there, and
/// a config naming another provider is read as the relay's.
#[test]
fn agents_run_through_the_sandbox_manager() {
    let defaults = Config::read(&RemoteWorkspace.defaults().config);
    assert_eq!(defaults.provider, "relay");
    let other = Config::read(
        &json!({"provider": "kubernetes", "provider_config": {"namespace": "agents"}}),
    );
    assert_eq!(other.provider, "relay");
    assert!(other.validate().is_err(), "the relay takes no namespace");
}
