//! Remote workspace agents as the Buzz app makes them: the presets offered,
//! the events that list an agent in every Buzz app, and the launch the
//! Sandbox manager registers at an agent's first start. Each shape is the
//! GPUI one, so an agent made here and one made in the app are the same to
//! every client and to the manager:
//!
//! - kind:0, signed by the agent: `{"name", "bot": true}` with the owner's
//!   NIP-OA `auth` tag (the agent host's `profile_event`).
//! - kind 30177, signed by the owner: `{"parallelism": 1, "respond_to":
//!   "owner-only", "name"}` with `d` = agent and `runtime` =
//!   [`RUNTIME_TYPE`] (`agents2/listings.rs`).
//! - kind 9000, signed by the owner: the agent joins the channel as `bot`.
//! - the first agent-signed `deploy` (`remote_workspace/config.rs`
//!   `Config::spec` through `sandbox-host`'s `payload`), which the manager
//!   stores as the agent's launch registration.

use std::collections::BTreeMap;

use nostr::{Event, EventBuilder, Keys, Kind, Tag};
use serde_json::{json, Value};
use uuid::Uuid;

use super::state::AgentRecord;
use crate::error::CliError;

/// The Remote workspace agent type, as the owner's record names it.
pub const RUNTIME_TYPE: &str = "voltage.remote-workspace/sandbox";
/// The owner's agent records.
pub const LISTING_KIND: u16 = 30177;

/// A harness the agent image runs.
pub struct Runtime {
    /// `claude`, `codex` or `buzz-agent`.
    pub id: &'static str,
    /// What the developer sees.
    pub label: &'static str,
    command: &'static str,
    default_env: &'static [(&'static str, &'static str)],
}

/// GPUI's `RUNTIMES`.
pub const RUNTIMES: &[Runtime] = &[
    Runtime {
        id: "claude",
        label: "Claude Code",
        command: "claude-agent-acp",
        default_env: &[],
    },
    Runtime {
        id: "codex",
        label: "Codex",
        command: "codex-acp",
        default_env: &[],
    },
    Runtime {
        id: "buzz-agent",
        label: "Buzz agent",
        command: "buzz-agent",
        default_env: &[("MCP_HOOK_SERVERS", "*")],
    },
];

/// A starting point for an agent.
pub struct Preset {
    /// What `--agents` takes.
    pub id: &'static str,
    /// The agent's display name.
    pub name: &'static str,
    /// One of [`RUNTIMES`].
    pub runtime: &'static str,
    /// Empty uses the harness default.
    pub model: &'static str,
    /// An effort GPUI offers: empty, low, medium, high, xhigh or max.
    pub effort: &'static str,
    /// The model credential it needs, as a `models` key.
    pub needs: &'static str,
    /// The harness system prompt.
    pub instructions: &'static str,
}

/// The team voltage/remote-agents.md describes ("Coordinate a team").
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "brokkr",
        name: "Brokkr",
        runtime: "claude",
        model: "",
        effort: "xhigh",
        needs: "claude_oauth_token",
        instructions: "You are Brokkr, the main builder and coordinator. Plan the work, \
            implement it, and run quality and security reviews with buzz-subagent review \
            before you open a pull request.",
    },
    Preset {
        id: "frigg",
        name: "Frigg",
        runtime: "claude",
        model: "",
        effort: "high",
        needs: "claude_oauth_token",
        instructions: "You are Frigg, the planner. Turn requests into concrete, ordered plans \
            with risks and validation steps. Do not implement unless asked.",
    },
    Preset {
        id: "bifrost",
        name: "Bifrost",
        runtime: "codex",
        model: "",
        effort: "high",
        needs: "codex_access_token",
        instructions: "You are Bifrost, an implementer. Make focused, tested changes and \
            report exactly what you changed and how you validated it.",
    },
    Preset {
        id: "forseti",
        name: "Forseti",
        runtime: "codex",
        model: "",
        effort: "xhigh",
        needs: "codex_access_token",
        instructions: "You are Forseti, a second opinion. Review designs and changes for \
            correctness and risk, and say plainly what you would block.",
    },
    Preset {
        id: "openrouter",
        name: "Odin",
        runtime: "buzz-agent",
        model: "qwen/qwen3-coder",
        effort: "",
        needs: "openrouter_api_key",
        instructions: "You are Odin, a general assistant running an OpenRouter model.",
    },
];

/// Whether a model credential that runs `needs` is among `available`
/// `models` keys: either one of each mutually exclusive pair serves its
/// harness, and the shared ChatGPT login serves Codex.
pub fn credential_available(needs: &str, available: &[String]) -> bool {
    let serves: &[&str] = match needs {
        "claude_oauth_token" => &["claude_oauth_token", "anthropic_api_key"],
        "codex_access_token" => &["codex_access_token", "openai_api_key", "codex_login"],
        _ => &[],
    };
    available
        .iter()
        .any(|key| key == needs || serves.contains(&key.as_str()))
}

/// The preset named `id`.
pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS
        .iter()
        .find(|preset| preset.id.eq_ignore_ascii_case(id.trim()))
}

fn runtime(id: &str) -> &'static Runtime {
    RUNTIMES
        .iter()
        .find(|runtime| runtime.id == id)
        .unwrap_or(&RUNTIMES[0])
}

/// A new agent from `preset`, with a fresh key the owner attests.
pub fn create(preset: &Preset, owner: &Keys) -> Result<AgentRecord, CliError> {
    let keys = Keys::generate();
    let auth_tag = buzz_sdk::nip_oa::compute_auth_tag(owner, &keys.public_key(), "")
        .map_err(|error| CliError::Other(format!("could not attest the agent: {error}")))?;
    let nsec = nostr::ToBech32::to_bech32(keys.secret_key())
        .map_err(|_| CliError::Other("could not encode the agent key".into()))?;
    Ok(AgentRecord {
        name: preset.name.into(),
        preset: preset.id.into(),
        runtime: preset.runtime.into(),
        model: preset.model.into(),
        effort: preset.effort.into(),
        instructions: preset.instructions.into(),
        pubkey: keys.public_key().to_hex(),
        secret: super::state::Secret::new(nsec),
        auth_tag,
        ..AgentRecord::default()
    })
}

/// The agent's keys.
pub fn keys(agent: &AgentRecord) -> Result<Keys, CliError> {
    Keys::parse(agent.secret.expose())
        .map_err(|_| CliError::Key(format!("the saved key of {} is invalid", agent.name)))
}

fn auth(agent: &AgentRecord) -> Result<Tag, CliError> {
    buzz_sdk::nip_oa::parse_auth_tag(&agent.auth_tag).map_err(|_| {
        CliError::Auth(format!(
            "the saved attestation of {} is invalid",
            agent.name
        ))
    })
}

/// The agent-signed kind:0 profile: `{"name", "bot": true}` and the owner's
/// attestation as its only tag.
pub fn profile_event(agent: &AgentRecord) -> Result<Event, CliError> {
    let content = json!({"name": agent.name, "bot": true}).to_string();
    EventBuilder::new(Kind::Metadata, content)
        .tags([auth(agent)?])
        .sign_with_keys(&keys(agent)?)
        .map_err(|error| CliError::Other(format!("signing failed: {error}")))
}

/// The owner-signed kind 30177 record that lists the agent in every Buzz
/// app as a Remote workspace agent.
pub fn listing_event(agent: &AgentRecord, owner: &Keys) -> Result<Event, CliError> {
    let content = json!({"parallelism": 1, "respond_to": "owner-only", "name": agent.name});
    let tags = [
        Tag::parse(["d", agent.pubkey.as_str()]),
        Tag::parse(["runtime", RUNTIME_TYPE]),
    ]
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .map_err(|error| CliError::Other(format!("tag error: {error}")))?;
    EventBuilder::new(Kind::Custom(LISTING_KIND), content.to_string())
        .tags(tags)
        .sign_with_keys(owner)
        .map_err(|error| CliError::Other(format!("signing failed: {error}")))
}

/// The owner-signed kind 9000 that adds the agent to `channel` as `bot`.
pub fn member_event(agent: &AgentRecord, channel: Uuid, owner: &Keys) -> Result<Event, CliError> {
    buzz_sdk::build_add_member(channel, &agent.pubkey, Some(buzz_sdk::MemberRole::Bot))
        .map_err(|error| CliError::Other(format!("build_add_member failed: {error}")))?
        .sign_with_keys(owner)
        .map_err(|error| CliError::Other(format!("signing failed: {error}")))
}

/// The owner's message that starts the agent's first thread: a mention, as
/// in the Buzz app.
pub fn kickoff_event(agent: &AgentRecord, channel: Uuid, owner: &Keys) -> Result<Event, CliError> {
    let content = format!(
        "@{} you were set up with `buzz onboard`. Reply in this thread to confirm that you are running.",
        agent.name
    );
    buzz_sdk::build_message(channel, &content, None, &[&agent.pubkey], false, &[], &[])
        .map_err(|error| CliError::Other(format!("build_message failed: {error}")))?
        .sign_with_keys(owner)
        .map_err(|error| CliError::Other(format!("signing failed: {error}")))
}

/// GPUI's `Config::spec` policy environment for `agent`.
pub fn policy_env(agent: &AgentRecord) -> BTreeMap<String, String> {
    let runtime = runtime(&agent.runtime);
    let mut env: BTreeMap<String, String> = runtime
        .default_env
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    let mut set = |key: &str, value: String| env.insert(key.into(), value);
    set("BUZZ_ACP_RELAY_OBSERVER", "true".into());
    set("BUZZ_ACP_LAZY_POOL", "true".into());
    set("BUZZ_ACP_AGENTS", "1".into());
    set("BUZZ_ACP_SESSION_POLICY", "thread".into());
    if !agent.instructions.trim().is_empty() {
        set("BUZZ_ACP_SYSTEM_PROMPT", agent.instructions.clone());
    }
    if !agent.model.trim().is_empty() {
        let key = match runtime.id {
            "claude" => "ANTHROPIC_MODEL",
            "buzz-agent" => "BUZZ_AGENT_MODEL",
            _ => "BUZZ_ACP_MODEL",
        };
        set(key, agent.model.trim().into());
    }
    if !agent.effort.is_empty() {
        set("BUZZ_ACP_EFFORT_LEVEL", agent.effort.clone());
    }
    let title: String = agent.name.chars().filter(|c| !c.is_control()).collect();
    if !title.trim().is_empty() {
        set("BUZZ_ACP_SESSION_TITLE", title.trim().into());
        set("BUZZ_ACP_DISPLAY_NAME", title.trim().into());
    }
    env
}

/// The provider `deploy` request the Buzz app's host sends through the relay
/// provider for a mention in `thread_root`: the agent's key, attestation and
/// launch, scoped to that thread, replaying from `replay_floor`.
pub fn deploy_payload(
    agent: &AgentRecord,
    relay: &str,
    owner: &str,
    channel: &str,
    thread_root: &str,
    replay_floor: u64,
) -> Value {
    let mut policy_env = policy_env(agent);
    policy_env.insert("BUZZ_ACP_REPLAY_FLOOR".into(), replay_floor.to_string());
    json!({
        "op": "deploy",
        "request_id": Uuid::new_v4().to_string(),
        "agent": {
            "name": agent.name,
            "relay_url": relay,
            "private_key_nsec": agent.secret.expose(),
            "auth_tag": agent.auth_tag,
            "env_vars": {},
            "launch": {
                "command": runtime(&agent.runtime).command,
                "args": [],
                "env": {},
                "policy_env": policy_env,
                "owner_pubkey": owner,
            },
        },
        "provider_config": {
            "sandbox": {"channel_id": channel, "thread_root": thread_root},
            "identity_policy": {"agent_pubkey": agent.pubkey, "owner_pubkey": owner},
        },
    })
}

/// The owner's Remote workspace agents, by pubkey and name, from their
/// kind 30177 records (newest per agent), as every Buzz app reads them.
pub fn remote_listings(events: &[Value], owner: &str) -> Vec<(String, String)> {
    let mut heads: BTreeMap<String, (u64, &Value)> = BTreeMap::new();
    for event in events {
        if event["kind"].as_u64() != Some(u64::from(LISTING_KIND))
            || event["pubkey"].as_str() != Some(owner)
        {
            continue;
        }
        let d = crate::client::extract_d_tag(event);
        let at = event["created_at"].as_u64().unwrap_or(0);
        if heads.get(&d).is_none_or(|(current, _)| at > *current) {
            heads.insert(d, (at, event));
        }
    }
    heads
        .into_iter()
        .filter(|(_, (_, event))| {
            crate::client::extract_tag_value(event, "runtime") == RUNTIME_TYPE
        })
        .map(|(pubkey, (_, event))| {
            let name = event["content"]
                .as_str()
                .and_then(|content| serde_json::from_str::<Value>(content).ok())
                .and_then(|content| content["name"].as_str().map(|name| name.trim().to_owned()))
                .unwrap_or_default();
            (pubkey, name)
        })
        .collect()
}
