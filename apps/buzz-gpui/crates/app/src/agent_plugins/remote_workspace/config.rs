//! The type's config, and the launch a session runs with: the desktop's
//! `build_launch_block_for_policy` for what this type configures.

use buzz_host::api::{Launch, LaunchSpec};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Runs each thread's Sandbox with the developer's own cluster access.
pub const KUBERNETES: &str = "kubernetes";
/// Asks Voltage's Sandbox manager to run it, over the relay.
pub const RELAY: &str = "relay";

/// The providers whose sessions belong to threads, as Settings names them.
pub const PROVIDERS: &[(&str, &str)] = &[
    (KUBERNETES, "Kubernetes"),
    (RELAY, "Sandbox manager (relay)"),
];

/// What the relay provider takes; the manager decides the rest.
const RELAY_FIELDS: &[&str] = &["relay_url", "manager_pubkey", "sandbox", "timeout_seconds"];

/// A harness the agent's image runs, as the desktop's catalog has it.
pub struct Runtime {
    pub id: &'static str,
    pub label: &'static str,
    /// Found on the image's `PATH`.
    command: &'static str,
    args: &'static [&'static str],
    default_env: &'static [(&'static str, &'static str)],
}

pub const RUNTIMES: &[Runtime] = &[
    Runtime {
        id: "claude",
        label: "Claude Code",
        command: "claude-agent-acp",
        args: &[],
        default_env: &[],
    },
    Runtime {
        id: "codex",
        label: "Codex",
        command: "codex-acp",
        args: &[],
        default_env: &[],
    },
    Runtime {
        id: "goose",
        label: "Goose",
        command: "goose",
        args: &["acp"],
        default_env: &[("GOOSE_MODE", "auto")],
    },
    Runtime {
        id: "buzz-agent",
        label: "Buzz agent",
        command: "buzz-agent",
        args: &[],
        default_env: &[("MCP_HOOK_SERVERS", "*")],
    },
];

/// What the type saves for each agent.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub provider: String,
    /// The provider's public config; always a thread Sandbox.
    pub provider_config: Value,
    /// The user's environment for the agent, which wins over the launch's.
    pub env: BTreeMap<String, String>,
    /// One of [`RUNTIMES`].
    pub runtime: String,
    /// Empty uses the harness's default.
    pub model: String,
    pub instructions: String,
    pub parallelism: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: KUBERNETES.into(),
            provider_config: json!({ "sandbox": true }),
            env: BTreeMap::new(),
            runtime: RUNTIMES[0].id.into(),
            model: String::new(),
            instructions: String::new(),
            parallelism: 1,
        }
    }
}

impl Config {
    /// The saved config; what is missing or malformed is the default.
    pub fn read(value: &Value) -> Self {
        let default = Self::default();
        let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
        let mut provider_config = value
            .get("provider_config")
            .filter(|config| config.is_object())
            .cloned()
            .unwrap_or(default.provider_config);
        provider_config["sandbox"] = Value::Bool(true);
        let env = value
            .get("env")
            .and_then(Value::as_object)
            .map(|env| {
                env.iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let runtime = text("runtime")
            .filter(|id| RUNTIMES.iter().any(|runtime| runtime.id == id))
            .unwrap_or(default.runtime);
        Self {
            provider: text("provider").unwrap_or(default.provider),
            provider_config,
            env,
            runtime,
            model: text("model").unwrap_or_default(),
            instructions: text("instructions").unwrap_or_default(),
            parallelism: value
                .get("parallelism")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
                .unwrap_or(default.parallelism),
        }
    }

    pub fn value(&self) -> Value {
        json!({
            "provider": self.provider,
            "provider_config": self.provider_config,
            "env": self.env,
            "runtime": self.runtime,
            "model": self.model,
            "instructions": self.instructions,
            "parallelism": self.parallelism,
        })
    }

    /// The relay provider's config: the manager at `manager_pubkey` on
    /// `relay_url`.
    pub fn relay(relay_url: &str, manager_pubkey: &str) -> Value {
        json!({
            "relay_url": relay_url.trim(),
            "manager_pubkey": manager_pubkey.trim(),
            "sandbox": true,
        })
    }

    /// Whether the host will accept it, as far as the GUI can tell.
    pub fn validate(&self) -> Result<(), String> {
        match self.provider.as_str() {
            KUBERNETES => {}
            RELAY => validate_relay(&self.provider_config)?,
            _ => return Err("Remote workspaces run on the kubernetes or relay provider.".into()),
        }
        buzz_remote_sessions::validate_provider_config(&self.provider_config)?;
        if let Some(key) = self.env.keys().find(|key| !env_name(key)) {
            return Err(format!("{key:?} is not an environment variable name."));
        }
        Ok(())
    }

    fn runtime(&self) -> &'static Runtime {
        RUNTIMES
            .iter()
            .find(|runtime| runtime.id == self.runtime)
            .unwrap_or(&RUNTIMES[0])
    }

    /// What the host launches for agent `name`. `team` is the agent's
    /// team section, if it has one.
    pub fn spec(&self, name: &str, team: Option<&str>) -> LaunchSpec {
        let runtime = self.runtime();
        let claude = runtime.id == "claude";
        let mut policy_env: BTreeMap<String, String> = runtime
            .default_env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let mut set = |key: &str, value: String| policy_env.insert(key.into(), value);
        set("BUZZ_ACP_RELAY_OBSERVER", "true".into());
        set("BUZZ_ACP_LAZY_POOL", "true".into());
        set("BUZZ_ACP_AGENTS", self.parallelism.to_string());
        // Each Sandbox is one thread's.
        set(SESSION_POLICY, "thread".into());
        if !self.instructions.trim().is_empty() {
            set("BUZZ_ACP_SYSTEM_PROMPT", self.instructions.clone());
        }
        if !self.model.trim().is_empty() {
            // Claude reads its startup model from ANTHROPIC_MODEL, and the Buzz
            // agent from BUZZ_AGENT_MODEL: it refuses to start without one.
            let key = match runtime.id {
                "claude" => "ANTHROPIC_MODEL",
                "buzz-agent" => "BUZZ_AGENT_MODEL",
                _ => "BUZZ_ACP_MODEL",
            };
            set(key, self.model.trim().into());
        }
        let title: String = name.chars().filter(|c| !c.is_control()).collect();
        if !title.trim().is_empty() {
            set("BUZZ_ACP_SESSION_TITLE", title.trim().into());
            set("BUZZ_ACP_DISPLAY_NAME", title.trim().into());
        }
        if let Some(team) = team.filter(|team| !team.trim().is_empty()) {
            set("BUZZ_ACP_TEAM_INSTRUCTIONS", team.into());
        }
        // The user's environment wins over policy_env, so it may not name
        // the session policy, nor for Claude a second startup model.
        let env = self
            .env
            .iter()
            .filter(|(key, _)| {
                !key.eq_ignore_ascii_case(SESSION_POLICY)
                    && !(claude
                        && (key.eq_ignore_ascii_case("BUZZ_ACP_MODEL")
                            || key.eq_ignore_ascii_case("ANTHROPIC_MODEL")))
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        LaunchSpec {
            provider: self.provider.clone(),
            provider_config_json: self.provider_config.to_string(),
            env,
            launch: Launch {
                command: runtime.command.into(),
                args: runtime.args.iter().map(|arg| arg.to_string()).collect(),
                env: BTreeMap::new(),
                policy_env,
            },
            name: name.into(),
        }
    }
}

const SESSION_POLICY: &str = "BUZZ_ACP_SESSION_POLICY";

/// The relay provider's schema: a `wss://` relay, the manager's key, and
/// an optional timeout of at most 15 minutes.
fn validate_relay(config: &Value) -> Result<(), String> {
    let config = config
        .as_object()
        .ok_or("The provider config must be a JSON object.")?;
    if let Some(key) = config
        .keys()
        .find(|key| !RELAY_FIELDS.contains(&key.as_str()))
    {
        return Err(format!(
            "The relay provider does not take {key}; the Sandbox manager decides it."
        ));
    }
    let relay = config.get("relay_url").and_then(Value::as_str);
    if !relay.is_some_and(wss_url) {
        return Err("The relay URL must be a wss:// address.".into());
    }
    let manager = config.get("manager_pubkey").and_then(Value::as_str);
    if !manager.is_some_and(lowercase_hex_key) {
        return Err("The manager key must be 64 lowercase hex characters.".into());
    }
    if let Some(timeout) = config.get("timeout_seconds")
        && !timeout.as_u64().is_some_and(|n| (1..=900).contains(&n))
    {
        return Err("The timeout must be 1 to 900 seconds.".into());
    }
    Ok(())
}

fn wss_url(text: &str) -> bool {
    url::Url::parse(text).is_ok_and(|url| url.scheme() == "wss" && url.host_str().is_some())
}

/// A Nostr public key as the provider takes it.
pub fn lowercase_hex_key(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `[A-Za-z_][A-Za-z0-9_]*`, which the provider requires.
fn env_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}
