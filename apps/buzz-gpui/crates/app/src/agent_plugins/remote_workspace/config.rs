//! The type's config, and the launch a session runs with: the desktop's
//! `build_launch_block_for_policy` for what this type configures.

use buzz_host::api::{Launch, LaunchSpec};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The only provider whose sessions belong to threads.
pub const PROVIDER: &str = "kubernetes";

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
            provider: PROVIDER.into(),
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

    /// Whether the host will accept it, as far as the GUI can tell.
    pub fn validate(&self) -> Result<(), String> {
        if self.provider != PROVIDER {
            return Err(format!(
                "Remote workspaces run on the {PROVIDER} provider only."
            ));
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
            // Claude reads its startup model from ANTHROPIC_MODEL.
            let key = if claude {
                "ANTHROPIC_MODEL"
            } else {
                "BUZZ_ACP_MODEL"
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

/// `[A-Za-z_][A-Za-z0-9_]*`, which the provider requires.
fn env_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}
