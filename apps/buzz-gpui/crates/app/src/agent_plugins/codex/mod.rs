//! Codex agents, buzz-app's bundled `buzz.codex` plugin: an Agents2 type
//! whose agents run Codex on the host that holds them, so they run on
//! remote buzzd hosts as well as this computer. The agents on one host
//! share one `codex app-server`; each conversation is one Codex thread,
//! carrying its agent's workspace, instructions and tools, which keeps
//! Codex's own coding prompt and tools. Codex acts in Buzz with the shared
//! Buzz tools as dynamic tools. Compiled in: there is no plugin loading.

pub mod config;
pub mod defaults;
pub mod history;
#[cfg(test)]
mod live;
pub mod prompt;
pub mod provider;
pub mod rpc;
pub mod runtime;
pub mod setup;
pub mod tabs;
pub mod transcript;

use crate::agent_plugins::managed::{Copies, UPDATE_EVERY};
use crate::agent_plugins::owner_relay;
use crate::agents2::{
    Agent, AgentTab, AgentType, Agents2, Delivery, MigrationSetting, Models, Status, Trigger,
    TypeDefaults, register_type,
};
use crate::hosts::{HostConnection, Hosts};
use crate::runtime::Runtime;
use config::Config;
use gpui_kit::*;
use provider::CodexProvider;
use runtime::{CodexRuntime, Run, Wake};
use std::sync::Arc;
use std::time::Duration;
use tabs::Host;

/// Registered as `buzz.codex/codex`, buzz-app's key.
pub const PLUGIN: &str = "buzz.codex";
pub const TYPE_ID: &str = "codex";
/// The agent type whose declared processes hosts run.
pub const PROCESS_OWNER: &str = "codex";
/// A run returns once Codex has the work, so this bounds only the
/// hand-over.
const HANDOVER: Duration = Duration::from_secs(2 * 60);

/// The runtime and Buzz's copy of Codex on each host, kept for the app's
/// lifetime.
pub struct Codex {
    runtime: CodexRuntime,
    /// Codex on this computer, for the Agents page's provider settings,
    /// and the device-wide defaults agents inherit.
    provider: Entity<CodexProvider>,
    copies: Copies,
    _updates: Task<()>,
    _subscriptions: Vec<Subscription>,
}

struct CodexGlobal(Entity<Codex>);

impl Global for CodexGlobal {}

/// Registers the type and starts its runtime. Call after `agents2::init`.
pub fn init(cx: &mut App) {
    // As in buzz-app: only macOS and Linux, since file uploads run
    // `base64`, which stock Windows lacks; no unusable type elsewhere.
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return;
    }
    if let Err(error) = register_type(PLUGIN, Arc::new(CodexType), cx) {
        tracing::error!(%error, "could not register the Codex agent type");
        return;
    }
    let state = cx.new(|cx| {
        let mut subscriptions = Vec::new();
        if let Some(agents) = Agents2::global(cx) {
            subscriptions.push(cx.observe(&agents, |this: &mut Codex, _, cx| this.sync(cx)));
        }
        subscriptions.push(
            cx.observe_global::<crate::plugins::Plugins>(|this: &mut Codex, cx| this.sync(cx)),
        );
        // Quitting ends every agent's server, and waits for them as long
        // as GPUI lets quit handlers run.
        subscriptions.push(cx.on_app_quit(|this: &mut Codex, cx| {
            let _runtime = Runtime::handle(cx).enter();
            this.runtime.dispose_and_wait()
        }));
        // The provider is Codex on the embedded host, sharing that host's
        // copy with its agents' Codex tabs.
        let mut copies = Copies::new(PROCESS_OWNER, setup::setup);
        let embedded = cx.try_global::<Hosts>().and_then(|hosts| {
            hosts
                .hosts
                .iter()
                .find(|host| host.read(cx).remote.is_none())
                .cloned()
        });
        let spawn = embedded
            .as_ref()
            .map(|host| host.read(cx).spawner(PROCESS_OWNER));
        let setup = match &embedded {
            Some(host) => copies.setup(host, cx),
            None => setup::setup(None, false),
        };
        let mut this = Codex {
            runtime: CodexRuntime::new(None),
            provider: cx.new(|cx| CodexProvider::new(spawn, setup, cx)),
            copies,
            _updates: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(UPDATE_EVERY).await;
                    if this.update(cx, |this, cx| this.copies.refresh(cx)).is_err() {
                        return;
                    }
                }
            }),
            _subscriptions: subscriptions,
        };
        this.sync(cx);
        this
    });
    cx.set_global(CodexGlobal(state));
}

fn key() -> String {
    format!("{PLUGIN}/{TYPE_ID}")
}

impl Codex {
    fn global(cx: &App) -> Option<Entity<Codex>> {
        cx.try_global::<CodexGlobal>()
            .map(|global| global.0.clone())
    }

    /// Agents of this type that are running keep their conversations:
    /// agents that stop running stop their turns, and a host's last one
    /// ends its server; switching communities or a reconnect does not.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let _runtime = Runtime::handle(cx).enter();
        // Turning the plugin off ends every server, as unloading it does in
        // buzz-app.
        if !crate::plugins::enabled(PLUGIN) {
            self.runtime.dispose();
            return;
        }
        let Some(agents) = Agents2::global(cx) else {
            return;
        };
        let agents = agents.read(cx);
        // A failed refresh still follows the running agents, so leaving a
        // community after one ends its servers.
        if *agents.status() == Status::Loading {
            return;
        }
        let key = key();
        let pubkeys: Vec<String> = agents
            .running()
            .iter()
            .filter(|agent| agent.agent_type == key)
            .map(|agent| agent.pubkey.clone())
            .collect();
        self.runtime.set_relays(owner_relay::running(agents, cx));
        self.runtime.sync(&pubkeys);
        self.copies.sync(agents, &key, cx);
    }
}

/// The agent's host, for its tabs.
fn host_of(pubkey: &str, cx: &mut App) -> Host {
    let host: Option<Entity<HostConnection>> =
        Agents2::global(cx).and_then(|agents| agents.read(cx).host_of(pubkey));
    let setup = match Codex::global(cx) {
        Some(state) => state.update(cx, |state, cx| state.copies.setup_of(pubkey, cx)),
        None => setup::setup(None, false),
    };
    Host {
        spawn: host.map(|host| host.read(cx).spawner(PROCESS_OWNER)),
        setup,
    }
}

struct CodexType;

/// The Codex tab before the runtime started, which `init` makes first.
struct Unavailable;

impl Render for Unavailable {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child("Codex agents run only in the desktop app")
    }
}

impl AgentType for CodexType {
    fn id(&self) -> &'static str {
        TYPE_ID
    }

    fn title(&self) -> SharedString {
        "Codex".into()
    }

    fn description(&self) -> Option<SharedString> {
        Some("Codex on this computer, using its native coding tools and your account.".into())
    }

    fn mark(&self) -> Option<buzz_ui::BrandMark> {
        Some(buzz_ui::BrandMark::Codex)
    }

    fn defaults(&self) -> TypeDefaults {
        TypeDefaults {
            config: Config::default().value(),
            attention: Default::default(),
        }
    }

    fn create_instructions(&self) -> bool {
        true
    }

    fn create_models(&self, cx: &mut App) -> Option<crate::agents2::CreateModels> {
        let provider = Codex::global(cx)?.read(cx).provider.clone();
        provider.update(cx, |provider, cx| provider.ensure_checked(cx));
        Some(provider.read(cx).create_models())
    }

    fn with_model(&self, config: serde_json::Value, model: &str, cx: &App) -> serde_json::Value {
        match Codex::global(cx) {
            Some(state) => state.read(cx).provider.read(cx).with_model(config, model),
            None => config,
        }
    }

    fn provider(&self, cx: &App) -> Option<std::rc::Rc<dyn crate::agents2::AgentProvider>> {
        let provider = Codex::global(cx)?.read(cx).provider.clone();
        Some(std::rc::Rc::new(provider::Provider(provider)))
    }

    fn adopts(&self) -> bool {
        true
    }

    /// A workspace Codex cannot use, such as a relative one, stays behind.
    fn adopt(&self, carry: &buzz_host::api::Carry, saved: serde_json::Value) -> serde_json::Value {
        let mut config = Config::read(&saved);
        if let Some(instructions) = &carry.instructions {
            config.instructions = instructions.clone();
        }
        if let Some(workspace) = carry
            .workspace
            .as_ref()
            .filter(|workspace| config::valid_workspace(workspace))
        {
            config.workspace = workspace.clone();
        }
        config.value()
    }

    fn carry(&self, saved: &serde_json::Value) -> buzz_host::api::Carry {
        let config = Config::read(saved);
        buzz_host::api::Carry {
            instructions: Some(config.instructions).filter(|text| !text.trim().is_empty()),
            workspace: Some(config.workspace)
                .filter(|workspace| *workspace != Config::default().workspace),
        }
    }

    fn describe_migration(&self, saved: &serde_json::Value) -> Vec<MigrationSetting> {
        let config = Config::read(saved);
        vec![
            MigrationSetting {
                id: "model",
                label: "Model",
                is_override: !config.model.trim().is_empty(),
                unchanged_key: None,
            },
            MigrationSetting {
                id: "thinking",
                label: "Thinking",
                is_override: !config.effort.is_empty(),
                unchanged_key: None,
            },
            MigrationSetting {
                id: "sessions",
                label: "Sessions",
                is_override: config.scope != Config::default().scope,
                unchanged_key: Some(config.scope),
            },
        ]
    }

    fn migration_warning(&self) -> Option<&'static str> {
        Some("Any commands Codex left running will stop.")
    }

    fn summary(&self, agent: &Agent) -> Option<String> {
        let model = Config::read(&agent.config).model;
        Some(if model.is_empty() {
            "Codex".into()
        } else {
            model
        })
    }

    fn tabs(&self) -> Vec<AgentTab> {
        vec![
            AgentTab {
                id: "codex".into(),
                title: "Codex".into(),
                build: Arc::new(|view, _, cx| {
                    let Some(state) = Codex::global(cx) else {
                        return cx.new(|_| Unavailable).into();
                    };
                    let (pubkey, conversation) = (view.agent.pubkey.clone(), view.conversation);
                    let host = host_of(&pubkey, cx);
                    let runtime = state.read(cx).runtime.clone();
                    cx.new(|cx| tabs::CodexTab::new(pubkey, conversation, host, runtime, cx))
                        .into()
                }),
            },
            AgentTab {
                id: "settings".into(),
                title: "Settings".into(),
                build: Arc::new(|view, window, cx| {
                    let host = host_of(&view.agent.pubkey, cx);
                    cx.new(|cx| tabs::SettingsTab::new(view, host, window, cx))
                        .into()
                }),
            },
        ]
    }

    fn runs(&self) -> bool {
        true
    }

    fn models(&self, agent: &Agent, cx: &mut App) -> Option<Task<Result<Models, String>>> {
        let state = Codex::global(cx)?;
        let host = Agents2::global(cx).and_then(|agents| agents.read(cx).host_of(&agent.pubkey))?;
        let spawn = host.read(cx).spawner(PROCESS_OWNER);
        let runtime = state.read(cx).runtime.clone();
        let (pubkey, config) = (agent.pubkey.clone(), agent.config.clone());
        let listed = Runtime::handle(cx)
            .spawn(async move { runtime.preview(&pubkey, &config, &spawn).await });
        Some(cx.background_spawn(async move {
            listed
                .await
                .unwrap_or_else(|_| Err("Codex stopped".to_string()))
        }))
    }

    fn run(&self, delivery: Delivery, cx: &mut App) -> Task<Result<(), String>> {
        tracing::debug!(agent = %delivery.agent.pubkey, "Codex delivery");
        if delivery.cancel.is_cancelled() {
            return Task::ready(Err("The run was cancelled".into()));
        }
        let Some(state) = Codex::global(cx) else {
            return Task::ready(Err("Codex agents run only in the desktop app".into()));
        };
        let host =
            Agents2::global(cx).and_then(|agents| agents.read(cx).host_of(&delivery.agent.pubkey));
        let Some(host) = host else {
            return Task::ready(Err("The agent's host is not connected".into()));
        };
        let spawn = host.read(cx).spawner(PROCESS_OWNER);
        let activity = super::activity(&delivery);
        let setup = state.update(cx, |state, cx| state.copies.setup(&host, cx));
        let cancel = delivery.cancel.clone();
        let wake = match delivery.trigger {
            Trigger::Mention { event } => Wake::Mention {
                event: super::event_of(&event),
            },
            Trigger::Watch {
                event, interest, ..
            } => Wake::Watch {
                event: super::event_of(&event),
                interest: interest.map(|interest| interest.instructions),
            },
            Trigger::Timer { .. } => Wake::Timer,
        };
        // Blank choices inherit the device defaults.
        let config = state.read(cx).provider.read(cx).resolve(&delivery.config);
        let run = Run {
            wake,
            channel: delivery.channel,
            agent: Arc::new(delivery.agent),
            config,

            spawn,
            model: delivery.model,
            effort: delivery.effort,
            activity,
            team: delivery.team.unwrap_or_default(),
            working: super::hold_until(delivery.working),
        };
        let runtime = state.read(cx).runtime.clone();
        // A mention during the first download waits for it, within its
        // deadline.
        let handed = Runtime::handle(cx).spawn(async move {
            tokio::select! {
                () = setup.installed() => {}
                () = cancel.cancelled() => return Err("The run was cancelled".to_string()),
            }
            runtime.run(run).await
        });
        cx.background_spawn(async move {
            handed
                .await
                .unwrap_or_else(|_| Err("Codex stopped".to_string()))
        })
    }

    fn timeout(&self) -> Duration {
        HANDOVER
    }
}

#[cfg(test)]
mod adopt_tests {
    use super::*;
    use crate::agents2::AgentType;
    use buzz_host::api::Carry;
    use core::prelude::v1::test;

    #[test]
    fn only_a_workspace_codex_can_use_comes_along() {
        let saved = Config::default().value();
        let carry = |workspace: &str| Carry {
            instructions: Some("Look around.".into()),
            workspace: Some(workspace.into()),
        };
        let adopted = Config::read(&CodexType.adopt(&carry("/srv/code"), saved.clone()));
        assert_eq!(
            (adopted.instructions.as_str(), adopted.workspace.as_str()),
            ("Look around.", "/srv/code")
        );
        let home = Config::read(&CodexType.adopt(&carry("~/code"), saved.clone()));
        assert_eq!(home.workspace, "~/code");
        let relative = Config::read(&CodexType.adopt(&carry("code"), saved));
        assert_eq!(relative.workspace, config::DEFAULT_WORKSPACE);
        assert_eq!(relative.instructions, "Look around.");
    }

    #[test]
    fn carries_instructions_and_workspace_and_describes_its_overrides() {
        let saved = Config::default().value();
        let blank = Config {
            model: "gpt".into(),
            instructions: " ".into(),
            ..Config::default()
        };
        assert_eq!(CodexType.carry(&blank.value()), Carry::default());
        let set = serde_json::json!({ "instructions": "Triage.", "workspace": "/work/code" });
        assert_eq!(
            CodexType.carry(&set),
            Carry {
                instructions: Some("Triage.".into()),
                workspace: Some("/work/code".into()),
            }
        );
        let overrides = |config: &serde_json::Value| {
            CodexType
                .describe_migration(config)
                .into_iter()
                .map(|setting| (setting.id, setting.is_override, setting.unchanged_key))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            overrides(&saved),
            [
                ("model", false, None),
                ("thinking", false, None),
                ("sessions", false, Some("thread".into())),
            ]
        );
        let custom = Config {
            model: "gpt-5".into(),
            effort: "high".into(),
            scope: "channel".into(),
            ..Config::default()
        };
        assert_eq!(
            overrides(&custom.value()),
            [
                ("model", true, None),
                ("thinking", true, None),
                ("sessions", true, Some("channel".into())),
            ]
        );
        assert_eq!(
            CodexType.migration_warning(),
            Some("Any commands Codex left running will stop.")
        );
    }
}
