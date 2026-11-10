//! `buzz onboard`: set up a developer's Buzz identity, credentials and
//! Remote workspace agents from one terminal command, without the Buzz app.
//!
//! The wizard sends exactly the events and Sandbox manager requests the Buzz
//! app sends, so what it creates appears and works in every Buzz app, and
//! what the app created is recognised here:
//!
//! 1. [`Wizard::identity`]: the owner key, the community invite, the profile.
//! 2. [`Wizard::github`]: the GitHub token, Git email and agent signing key.
//! 3. [`Wizard::models`]: Claude, Codex and OpenRouter credentials.
//! 4. [`Wizard::op_token`]: the 1Password service account token.
//! 5. [`Wizard::agents`]: agents from presets, listed and added to a channel.
//! 6. [`Wizard::enroll`]: the encrypted `enroll`, the signing key on GitHub,
//!    and the Paseo pairing; then [`Wizard::first_start`] registers each new
//!    agent with the manager by its first agent-signed start in a channel
//!    thread, which is also what lists it for the Paseo daemon.
//! 7. [`Wizard::gateway_grant`]: the owner's expiring grant of the Paseo
//!    daemon's gateway key ([`gateway`]).
//! 8. [`Wizard::next_steps`]: Paseo pairing, workflows, the Buzz app, and
//!    what still needs the developer.
//!
//! Each step is idempotent: progress is saved after every durable write
//! ([`state`]), and a re-run skips what is done. Credentials are held in
//! memory only and sent encrypted to the manager.

mod agents;
mod gateway;
mod io;
mod manager;
mod paseo;
mod profile;
mod relay;
mod state;
mod steps_agents;
mod steps_credentials;
mod steps_enroll;
mod steps_gateway;
mod steps_identity;
mod tools;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use nostr::PublicKey;
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::error::CliError;
use io::Prompter;
use manager::ControlTransport;
use relay::Relay;
use state::{State, Store};
use tools::Tools;

/// The Voltage community.
pub const DEFAULT_RELAY: &str = "wss://buzz.voltage.cloud";

/// `buzz onboard` arguments. Secrets are read only from the environment or
/// hidden prompts, never from flags, so they stay out of process listings.
#[derive(clap::Args, Debug, Clone)]
pub struct OnboardArgs {
    /// Community relay. Only wss:// is accepted.
    #[arg(long, default_value = DEFAULT_RELAY)]
    pub relay: String,
    /// Never prompt: take every answer from flags and BUZZ_ONBOARD_* variables.
    #[arg(long)]
    pub yes: bool,
    /// Import an existing key from BUZZ_PRIVATE_KEY (or a hidden prompt)
    /// instead of generating one.
    #[arg(long)]
    pub import_key: bool,
    /// Invite code or invite URL, when the key is not a member yet.
    #[arg(long, env = "BUZZ_ONBOARD_INVITE", hide_env_values = true)]
    pub invite: Option<String>,
    /// Accept the community's join policy without asking.
    #[arg(long)]
    pub accept_join_policy: bool,
    /// Confirm the minimum age, when the join policy requires it.
    #[arg(long)]
    pub confirm_age: bool,
    /// Your display name, when you have no profile yet.
    #[arg(long)]
    pub name: Option<String>,
    /// A verified email on your GitHub account [default: git config --global user.email].
    #[arg(long)]
    pub git_email: Option<String>,
    /// A via profile TOML file [default: the shared Voltage via profile].
    #[arg(long)]
    pub via_profile: Option<PathBuf>,
    /// Agents to create, by preset (brokkr, frigg, bifrost, forseti, openrouter).
    #[arg(long, value_delimiter = ',')]
    pub agents: Vec<String>,
    /// The workspace channel for the agents: a UUID or a name (created when missing).
    #[arg(long)]
    pub channel: Option<String>,
    /// Enroll again even when a profile is on file (keeps what you leave blank).
    #[arg(long)]
    pub reenroll: bool,
    /// Create a new agent signing key when enrolling again.
    #[arg(long)]
    pub replace_signing_key: bool,
    /// Do not start new agents once to register them with the Sandbox manager.
    #[arg(long)]
    pub skip_first_start: bool,
    /// How long the Paseo daemon's gateway grant lasts: hours, days or weeks
    /// (36h, 90d, 2w), at most 90d.
    #[arg(long, default_value = gateway::DEFAULT_LIFETIME)]
    pub gateway_expires: String,
    /// Do not grant the Paseo daemon's gateway key.
    #[arg(long)]
    pub skip_gateway_grant: bool,
    /// Where onboarding state and the key are kept [default: <config dir>/buzz].
    #[arg(long, env = "BUZZ_ONBOARD_DIR")]
    pub state_dir: Option<PathBuf>,
    /// The Sandbox manager's public key, hex.
    #[arg(long, default_value = manager::MANAGER_PUBKEY, hide = true)]
    pub manager_pubkey: String,
}

/// The secret inputs that may come from the environment.
#[derive(Default)]
pub struct EnvSecrets {
    /// `BUZZ_PRIVATE_KEY`, read only with `--import-key`.
    pub private_key: Option<Zeroizing<String>>,
    /// `BUZZ_ONBOARD_GITHUB_TOKEN`.
    pub github_token: Option<Zeroizing<String>>,
    /// `BUZZ_ONBOARD_OP_TOKEN`.
    pub op_token: Option<Zeroizing<String>>,
    /// `BUZZ_ONBOARD_CLAUDE_TOKEN`.
    pub claude_token: Option<Zeroizing<String>>,
    /// `BUZZ_ONBOARD_CODEX_TOKEN`.
    pub codex_token: Option<Zeroizing<String>>,
    /// `BUZZ_ONBOARD_OPENROUTER_KEY`.
    pub openrouter_key: Option<Zeroizing<String>>,
    /// `BUZZ_ONBOARD_OPENAI_API_KEY`.
    pub openai_api_key: Option<Zeroizing<String>>,
    /// `BUZZ_ONBOARD_ANTHROPIC_API_KEY`.
    pub anthropic_api_key: Option<Zeroizing<String>>,
}

impl EnvSecrets {
    /// Reads the variables; empty ones count as unset.
    pub fn from_env(import_key: bool) -> Self {
        let read = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| Zeroizing::new(value.trim().to_owned()))
                .filter(|value| !value.is_empty())
        };
        Self {
            private_key: import_key.then(|| read("BUZZ_PRIVATE_KEY")).flatten(),
            github_token: read("BUZZ_ONBOARD_GITHUB_TOKEN"),
            op_token: read("BUZZ_ONBOARD_OP_TOKEN"),
            claude_token: read("BUZZ_ONBOARD_CLAUDE_TOKEN"),
            codex_token: read("BUZZ_ONBOARD_CODEX_TOKEN"),
            openrouter_key: read("BUZZ_ONBOARD_OPENROUTER_KEY"),
            openai_api_key: read("BUZZ_ONBOARD_OPENAI_API_KEY"),
            anthropic_api_key: read("BUZZ_ONBOARD_ANTHROPIC_API_KEY"),
        }
    }
}

/// The community as a canonical `wss://host[:port]` origin. Anything but
/// `wss://` is refused: the owner key and every credential travel over it.
pub fn canonical_relay(text: &str) -> Result<String, CliError> {
    let refuse = || {
        CliError::Usage(format!(
            "--relay must be a wss:// community origin such as {DEFAULT_RELAY}"
        ))
    };
    let url = url::Url::parse(text.trim()).map_err(|_| refuse())?;
    if url.scheme() != "wss"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(refuse());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// The wizard: the saved state, the options, and its boundaries.
pub struct Wizard<P, R, T, X> {
    args: OnboardArgs,
    env: EnvSecrets,
    relay_url: String,
    manager: PublicKey,
    store: Store,
    state: State,
    prompter: P,
    relay: R,
    transport: T,
    tools: X,
    /// Signs gateway grants; [`gateway::Unreleased`] until the SDK lands.
    grants: Box<dyn gateway::GatewayGrants + Send + Sync>,
    /// `--gateway-expires`: its canonical text and seconds.
    gateway_lifetime: (String, u64),
    /// Things the developer must still do; reported, never silent.
    actions: Vec<String>,
    /// The `models` keys on file or about to be enrolled.
    available_models: Vec<String>,
}

impl<P: Prompter, R: Relay, T: ControlTransport, X: Tools> Wizard<P, R, T, X> {
    /// A wizard over `store` and the given boundaries.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        args: OnboardArgs,
        env: EnvSecrets,
        store: Store,
        prompter: P,
        relay: R,
        transport: T,
        tools: X,
    ) -> Result<Self, CliError> {
        let relay_url = canonical_relay(&args.relay)?;
        let gateway_lifetime = gateway::lifetime(&args.gateway_expires).map_err(CliError::Usage)?;
        let manager = PublicKey::from_hex(&args.manager_pubkey)
            .map_err(|_| CliError::Usage("the manager key must be 64 hex characters".into()))?;
        let mut state = store.load().map_err(CliError::Other)?;
        if state.relay.is_empty() {
            state.relay = relay_url.clone();
        } else if state.relay != relay_url {
            return Err(CliError::Usage(format!(
                "{} holds onboarding for {}; pass --state-dir to onboard another community",
                store.dir().display(),
                state.relay
            )));
        }
        Ok(Self {
            args,
            env,
            relay_url,
            manager,
            store,
            state,
            prompter,
            relay,
            transport,
            tools,
            grants: Box::new(gateway::Unreleased),
            gateway_lifetime,
            actions: Vec::new(),
            available_models: Vec::new(),
        })
    }

    fn save(&self) -> Result<(), CliError> {
        self.store.save(&self.state).map_err(CliError::Other)
    }

    fn say(&mut self, text: &str) {
        self.prompter.say(text);
    }

    /// Runs every step in order and returns the summary. A step that fails
    /// stops the run with its error; everything before it stays saved. A
    /// failed first start or gateway grant still shows the next steps, then
    /// fails the run.
    pub async fn run(&mut self) -> Result<Value, CliError> {
        self.say("Step 1 of 8: Buzz identity");
        let keys = self.identity().await?;
        let on_file = self.on_file(&keys).await?;
        let mut credentials = steps_credentials::Credentials::default();
        self.say("Step 2 of 8: GitHub");
        self.github(&on_file, &mut credentials).await?;
        self.say("Step 3 of 8: Model credentials");
        self.models(&on_file, &mut credentials).await?;
        self.say("Step 4 of 8: 1Password service account");
        self.op_token(&on_file, &mut credentials)?;
        self.say("Step 5 of 8: Agents");
        let login = credentials.login.clone();
        self.agents(&keys, &login).await?;
        self.say("Step 6 of 8: Enroll your profile");
        let pairing = self.enroll(&keys, &on_file, credentials).await?;
        let started = self.first_start(&keys).await;
        self.say("Step 7 of 8: Paseo gateway grant");
        let granted = self.gateway_grant(&keys, pairing.as_ref()).await;
        self.say("Step 8 of 8: Next steps");
        self.next_steps(pairing.as_ref());
        let summary = self.summary(pairing.as_ref());
        started.and(granted).map(|()| summary)
    }

    /// What was set up, without any secret.
    fn summary(&self, pairing: Option<&paseo::Pairing>) -> Value {
        json!({
            "relay": self.relay_url,
            "owner_pubkey": self.state.owner_pubkey,
            "state_dir": self.store.dir().display().to_string(),
            "channel": self.state.channel.as_ref().map(|channel| json!({
                "id": channel.id, "name": channel.name,
            })),
            "agents": self.state.agents.iter().map(|agent| json!({
                "name": agent.name,
                "pubkey": agent.pubkey,
                "runtime": agent.runtime,
                "registered": agent.registered,
            })).collect::<Vec<_>>(),
            "enrolled_version": self.state.enrolled_version,
            "paseo": pairing.map_or(json!({"provisioned": false}), |pairing| json!({
                "provisioned": true,
                "server_id": pairing.server_id,
                "offer_url": pairing.offer_url,
                "gateway_pubkey": pairing.gateway.map(|key| key.to_hex()),
            })),
            "gateway_grant": self.state.gateway_grant.as_ref().map(|grant| json!({
                "gateway_pubkey": grant.gateway, "expires_at": grant.expires_at,
            })),
            "action_needed": self.actions,
        })
    }
}

/// `buzz onboard` with the real terminal, relay, manager and tools.
pub async fn run(args: OnboardArgs) -> Result<(), CliError> {
    let dir = args
        .state_dir
        .clone()
        .or_else(Store::default_dir)
        .ok_or_else(|| CliError::Usage("no config directory: pass --state-dir".into()))?;
    let relay_url = canonical_relay(&args.relay)?;
    let env = EnvSecrets::from_env(args.import_key);
    let prompter = io::Terminal::new(args.yes);
    let mut wizard = Wizard::new(
        args,
        env,
        Store::new(dir),
        prompter,
        relay::HttpRelay::new(&relay_url),
        manager::WsTransport::new(&relay_url),
        tools::SystemTools,
    )?;
    let summary = wizard.run().await?;
    println!("{summary}");
    Ok(())
}
