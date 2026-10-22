//! Remote workspaces for an agent host: thread-bound sessions on a
//! `buzz-backend-*` provider, `buzz-backend-kubernetes` or
//! `buzz-backend-relay`, run through `buzz-remote-sessions` as the desktop
//! runs them.
//!
//! The GUI describes the launch. The host adds what only it holds, the
//! agent's community, owner, attestation and key, and refuses them from the
//! GUI. Each operation holds the agent's lock, so two never overlap, and
//! rebuilds the payload under it, so a deleted agent never launches.

use buzz_host::api::{
    EndState, LaunchSpec, RecoveryMode, SandboxProvider, SandboxRequest, SandboxResponse,
    SandboxSession,
};
use buzz_host::sandbox::{AgentCustody, LaunchIdentity, SandboxBackend};
use buzz_remote_sessions::{
    self as remote, AgentLocks, Discovery, Invocation, InvocationSource, ProviderTarget,
    RemoteSessions, SessionError, SessionScope, StopState, Tenant,
};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Environment only the host sets, from custody. The provider expects them
/// gone from what it is given.
const RESERVED_ENV: &[&str] = &[
    "BUZZ_PRIVATE_KEY",
    "NOSTR_PRIVATE_KEY",
    "BUZZ_AUTH_TAG",
    "BUZZ_RELAY_URL",
];

/// Provider config the host fills from custody and the session.
const RESERVED_CONFIG: &[&str] = &[
    "identity_policy",
    "relay_url",
    "private_key_nsec",
    "auth_tag",
    "owner_pubkey",
];

/// The relay provider's own `relay_url`: where it reaches the Sandbox
/// manager, not the agent's community.
const RELAY_PROVIDER: &str = "relay";

/// Runs a host's remote workspaces. One per host, so every connection
/// shares its per-agent locks.
pub struct Sandboxes {
    locks: AgentLocks,
    /// Where providers run.
    workdir: PathBuf,
    /// Where providers are found; `None` for beside this executable, then
    /// `PATH` and `~/.local/bin`.
    dirs: Option<Vec<PathBuf>>,
}

impl Sandboxes {
    /// Providers run in `<state_dir>/sandbox`.
    pub fn new(state_dir: &Path) -> Self {
        Self {
            locks: AgentLocks::default(),
            workdir: state_dir.join("sandbox"),
            dirs: None,
        }
    }

    /// Finds providers only in `dirs`.
    pub fn with_provider_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.dirs = Some(dirs);
        self
    }

    /// Looked up for each operation, so a provider installed later is found.
    fn discovery(&self) -> Discovery {
        match &self.dirs {
            Some(dirs) => Discovery { dirs: dirs.clone() },
            None => Discovery::from_env(
                std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.parent().map(Path::to_path_buf)),
            ),
        }
    }
}

impl SandboxBackend for Sandboxes {
    fn serve(
        &self,
        request: SandboxRequest,
        custody: Arc<dyn AgentCustody>,
    ) -> BoxFuture<'static, SandboxResponse> {
        let context = Context {
            locks: self.locks.clone(),
            discovery: self.discovery(),
            workdir: self.workdir.clone(),
            custody,
        };
        Box::pin(async move {
            context
                .serve(request)
                .await
                .unwrap_or_else(|error| SandboxResponse::Failed {
                    message: error.message,
                    code: error.code,
                })
        })
    }
}

/// What one request runs with.
struct Context {
    locks: AgentLocks,
    discovery: Discovery,
    workdir: PathBuf,
    custody: Arc<dyn AgentCustody>,
}

impl Context {
    async fn serve(self, request: SandboxRequest) -> Result<SandboxResponse, SessionError> {
        std::fs::create_dir_all(&self.workdir)
            .map_err(|error| format!("Could not make the remote workspace directory: {error}"))?;
        match request {
            SandboxRequest::Providers => Ok(SandboxResponse::Providers(self.providers().await)),
            SandboxRequest::Lifecycle { pubkey, spec } => {
                let sessions = self.sessions(spec)?;
                Ok(SandboxResponse::Lifecycle(
                    sessions.supports_lifecycle(&pubkey).await?,
                ))
            }
            SandboxRequest::Start {
                session,
                replay_floor,
            } => {
                let (thread, sessions) = self.prepare(session).await?;
                sessions
                    .deploy(
                        &thread.pubkey,
                        thread.tenant(),
                        Some(&thread.scope),
                        replay_floor,
                    )
                    .await?;
                Ok(SandboxResponse::Started)
            }
            SandboxRequest::End { session } => {
                let (thread, sessions) = self.prepare(session).await?;
                let state = sessions
                    .end(&thread.pubkey, thread.tenant(), &thread.scope)
                    .await?;
                Ok(SandboxResponse::Ended {
                    state: match state {
                        StopState::Ending => EndState::Ending,
                        StopState::Ended => EndState::Ended,
                        StopState::Absent => EndState::Absent,
                    },
                })
            }
            SandboxRequest::Recover { session, mode } => {
                let (thread, sessions) = self.prepare(session).await?;
                let mode = match mode {
                    RecoveryMode::Checkpoint => remote::RecoveryMode::Checkpoint,
                    RecoveryMode::Fresh => remote::RecoveryMode::Fresh,
                };
                sessions
                    .recover(&thread.pubkey, thread.tenant(), &thread.scope, mode)
                    .await?;
                Ok(SandboxResponse::Started)
            }
        }
    }

    /// Checks the session and reads the agent's key, which may wait on the
    /// credential store, before the agent's lock is taken.
    async fn prepare(
        self,
        session: SandboxSession,
    ) -> Result<(Thread, RemoteSessions<Source>), SessionError> {
        let SandboxSession {
            pubkey,
            tenant,
            scope,
            spec,
        } = session;
        if tenant.relay.trim().is_empty() || tenant.signer.trim().is_empty() {
            return Err("A remote workspace needs the community and person it is for".into());
        }
        let channel = scope
            .channel_id
            .parse()
            .map_err(|_| "A remote workspace needs a channel's UUID")?;
        let scope = SessionScope::new(channel, scope.thread_root)?;
        self.custody.launch_identity(&pubkey).await?;
        let thread = Thread {
            pubkey,
            relay: tenant.relay,
            signer: tenant.signer,
            scope,
        };
        Ok((thread, self.sessions(spec)?))
    }

    fn sessions(self, spec: LaunchSpec) -> Result<RemoteSessions<Source>, SessionError> {
        let source = Source::new(self.custody, spec)?;
        Ok(RemoteSessions::new(
            source,
            self.locks,
            self.discovery,
            Some(self.workdir),
        ))
    }

    /// Every provider found that answers `info`.
    async fn providers(self) -> Vec<SandboxProvider> {
        let Self {
            discovery, workdir, ..
        } = self;
        tokio::task::spawn_blocking(move || {
            discovery
                .candidates()
                .into_iter()
                .filter_map(|(id, path)| {
                    let info = remote::provider_info(&path, Some(&workdir)).ok()?;
                    Some(SandboxProvider {
                        id,
                        path: path.display().to_string(),
                        version: info.version,
                        ops: info.ops,
                        config_schema_json: info.config_schema.to_string(),
                    })
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }
}

/// The agent and thread one session request is for.
struct Thread {
    pubkey: String,
    relay: String,
    signer: String,
    scope: SessionScope,
}

impl Thread {
    fn tenant(&self) -> Tenant<'_> {
        Tenant {
            relay_url: Some(&self.relay),
            signer_pubkey: Some(&self.signer),
        }
    }
}

/// One request's agent record: the GUI's launch spec, and custody for the
/// rest.
struct Source {
    custody: Arc<dyn AgentCustody>,
    spec: LaunchSpec,
    config: Value,
}

impl Source {
    fn new(custody: Arc<dyn AgentCustody>, spec: LaunchSpec) -> Result<Self, SessionError> {
        let config = provider_config(&spec)?;
        Ok(Self {
            custody,
            spec,
            config,
        })
    }
}

impl InvocationSource for Source {
    fn target(&self, _: &str) -> Result<Option<ProviderTarget>, SessionError> {
        Ok(Some(ProviderTarget {
            provider_id: self.spec.provider.clone(),
            provider_config: self.config.clone(),
            cached_binary: None,
        }))
    }

    /// Runs under the agent's lock, so it reads only the key read before
    /// it: a deleted agent fails here.
    fn rebuild(&self, pubkey: &str) -> Result<Invocation, SessionError> {
        let identity = self.custody.cached_launch_identity(pubkey)?;
        Ok(Invocation {
            target: self.target(pubkey)?.expect("always a provider"),
            payload: payload(&self.spec, &identity),
        })
    }
}

/// The spec's provider config: public, a thread Sandbox, and without what
/// the host fills in.
fn provider_config(spec: &LaunchSpec) -> Result<Value, String> {
    let config: Value = serde_json::from_str(&spec.provider_config_json)
        .map_err(|_| "The provider config is not valid JSON")?;
    remote::validate_provider_config(&config)?;
    let reserved = |key: &str| !(spec.provider == RELAY_PROVIDER && key == "relay_url");
    if let Some(key) = RESERVED_CONFIG
        .iter()
        .find(|key| reserved(key) && config.get(**key).is_some())
    {
        return Err(format!(
            "The provider config may not set {key}; the host does"
        ));
    }
    if !remote::is_thread_sandbox(&spec.provider, &config) {
        return Err(
            "Remote workspaces need the kubernetes or relay provider with sandbox: true".into(),
        );
    }
    Ok(config)
}

/// The provider's deploy payload (`agent` on the wire): the community,
/// key, attestation and owner from custody, and the rest from the spec.
/// The user's environment wins over the launch's.
fn payload(spec: &LaunchSpec, identity: &LaunchIdentity) -> Value {
    let user = unreserved(&spec.env);
    let mut env = unreserved(&spec.launch.env);
    env.extend(user.clone());
    json!({
        "name": spec.name,
        "relay_url": identity.relay,
        "private_key_nsec": identity.nsec.as_str(),
        "auth_tag": identity.auth,
        "env_vars": user,
        "launch": {
            "command": spec.launch.command,
            "args": spec.launch.args,
            "env": env,
            "policy_env": unreserved(&spec.launch.policy_env),
            "owner_pubkey": identity.owner,
        },
    })
}

fn unreserved(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.iter()
        .filter(|(key, _)| !RESERVED_ENV.iter().any(|r| key.eq_ignore_ascii_case(r)))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_host::api::Launch;

    fn identity() -> LaunchIdentity {
        LaunchIdentity {
            pubkey: "a".repeat(64),
            relay: "wss://relay.example".into(),
            owner: "b".repeat(64),
            auth: r#"["auth","bb","","sig"]"#.into(),
            nsec: String::from("nsec1agent").into(),
        }
    }

    fn spec(config: Value) -> LaunchSpec {
        LaunchSpec {
            provider: "kubernetes".into(),
            provider_config_json: config.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_payload_takes_identity_from_custody_only() {
        let env = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect()
        };
        let spec = LaunchSpec {
            env: env(&[("buzz_private_key", "gui"), ("API_TOKEN", "user")]),
            launch: Launch {
                command: "claude-agent-acp".into(),
                env: env(&[("API_TOKEN", "launch"), ("BUZZ_RELAY_URL", "wss://evil")]),
                policy_env: env(&[("BUZZ_AUTH_TAG", "gui"), ("BUZZ_ACP_LAZY_POOL", "true")]),
                ..Default::default()
            },
            ..spec(json!({ "sandbox": true }))
        };
        let payload = payload(&spec, &identity());
        assert_eq!(payload["relay_url"], "wss://relay.example");
        assert_eq!(payload["private_key_nsec"], "nsec1agent");
        assert_eq!(payload["launch"]["owner_pubkey"], "b".repeat(64));
        assert_eq!(payload["env_vars"], json!({ "API_TOKEN": "user" }));
        assert_eq!(payload["launch"]["env"], json!({ "API_TOKEN": "user" }));
        assert_eq!(
            payload["launch"]["policy_env"],
            json!({ "BUZZ_ACP_LAZY_POOL": "true" })
        );
    }

    #[test]
    fn the_provider_config_is_a_thread_sandbox_without_host_fields() {
        assert!(provider_config(&spec(json!({ "sandbox": true }))).is_ok());
        for config in [
            json!({ "sandbox": true, "identity_policy": {} }),
            json!({ "sandbox": true, "relay_url": "wss://evil" }),
            json!({ "sandbox": false }),
            json!({ "sandbox": true, "token": "secret" }),
        ] {
            assert!(provider_config(&spec(config.clone())).is_err(), "{config}");
        }
        let other = LaunchSpec {
            provider: "docker".into(),
            ..spec(json!({ "sandbox": true }))
        };
        assert!(provider_config(&other).is_err());
    }

    #[test]
    fn only_the_relay_provider_names_its_relay() {
        let config = json!({
            "sandbox": true, "relay_url": "wss://relay.example", "manager_pubkey": "c".repeat(64),
        });
        let relay = LaunchSpec {
            provider: "relay".into(),
            ..spec(config.clone())
        };
        assert_eq!(provider_config(&relay).unwrap(), config);
        assert!(provider_config(&spec(config)).is_err(), "kubernetes");
        for key in [
            "identity_policy",
            "private_key_nsec",
            "auth_tag",
            "owner_pubkey",
        ] {
            let mut config = json!({ "sandbox": true });
            config[key] = json!("x");
            let relay = LaunchSpec {
                provider: "relay".into(),
                ..spec(config)
            };
            assert!(provider_config(&relay).is_err(), "{key}");
        }
    }
}
