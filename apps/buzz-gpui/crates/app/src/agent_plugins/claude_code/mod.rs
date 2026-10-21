//! Claude agents, buzz-app's `src/agent-plugins/claude-code`: an
//! Agents2 type whose agents are Claude sessions on the host that holds
//! them, so they run on remote buzzd hosts as well as this computer. Each
//! conversation the agent is in is one continuing session; Claude answers
//! with the in-process Buzz tools. Compiled in: there is no plugin loading.

pub mod claude;
pub mod defaults;
#[cfg(debug_assertions)]
mod dev;
#[cfg(test)]
mod e2e;
pub mod lanes;
pub mod prompt;
pub mod provider;
pub mod runtime;
pub mod sessions;
pub mod setup;
pub mod tabs;
pub mod transcript;

use crate::agent_plugins::managed::{Copies, UPDATE_EVERY};
use crate::agent_plugins::owner_relay;
use crate::agent_plugins::process::Spawner;
use crate::agents2::{
    Agent, AgentTab, AgentType, Agents2, Delivery, MigrationSetting, Models, Status, Trigger,
    TypeDefaults, register_type,
};
use crate::hosts::Hosts;
use crate::runtime::Runtime;
use gpui_kit::*;
use provider::{ClaudeProvider, DefaultsChanged};
use runtime::{ClaudeRuntime, Config, Run, Synced, Wake};
use std::sync::Arc;
use std::time::Duration;

/// Registered as `buzz.claude-code/claude-code`, buzz-app's key.
pub const PLUGIN: &str = "buzz.claude-code";
pub const TYPE_ID: &str = "claude-code";
/// The agent type whose declared processes hosts run.
pub const PROCESS_OWNER: &str = "claude-code";
/// A turn's run returns once Claude has it, so this bounds only the
/// hand-over.
const HANDOVER: Duration = Duration::from_secs(2 * 60);

/// The runtime and each host's setup view, kept for the app's lifetime.
pub struct ClaudeCode {
    runtime: ClaudeRuntime,
    /// Buzz's copy of Claude on each host.
    copies: Copies,
    _updates: Task<()>,
    /// Claude on this computer, for the Agents page's provider settings,
    /// and the device-wide defaults agents inherit.
    provider: Entity<ClaudeProvider>,
    _subscriptions: Vec<Subscription>,
}

struct ClaudeCodeGlobal(Entity<ClaudeCode>);

impl Global for ClaudeCodeGlobal {}

/// Registers the type and starts its runtime. Call after `agents2::init`.
pub fn init(cx: &mut App) {
    if let Err(error) = register_type(PLUGIN, Arc::new(ClaudeCodeType), cx) {
        tracing::error!(%error, "could not register the Claude agent type");
        return;
    }
    let state = cx.new(|cx| {
        let mut subscriptions = Vec::new();
        if let Some(agents) = Agents2::global(cx) {
            subscriptions.push(cx.observe(&agents, |this: &mut ClaudeCode, _, cx| this.sync(cx)));
        }
        subscriptions.push(
            cx.observe_global::<crate::plugins::Plugins>(|this: &mut ClaudeCode, cx| this.sync(cx)),
        );
        // Quitting stops every session's process, as buzz-app does on exit,
        // and waits for them as long as GPUI lets quit handlers run.
        subscriptions.push(cx.on_app_quit(|this: &mut ClaudeCode, cx| {
            let _runtime = Runtime::handle(cx).enter();
            this.runtime.dispose_and_wait()
        }));
        // The provider is Claude on the embedded host, sharing that host's
        // copy with its agents' Claude tabs.
        let mut copies = Copies::new(PROCESS_OWNER, setup::for_host);
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
            None => setup::setup(None),
        };
        let provider = cx.new(|cx| ClaudeProvider::new(setup, spawn, cx));
        // Agents with a blank model follow a changed default.
        subscriptions.push(cx.subscribe(
            &provider,
            |this: &mut ClaudeCode, _, _: &DefaultsChanged, cx| this.sync(cx),
        ));
        let mut this = ClaudeCode {
            runtime: ClaudeRuntime::new(None),
            copies,
            _updates: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(UPDATE_EVERY).await;
                    if this.update(cx, |this, cx| this.copies.refresh(cx)).is_err() {
                        return;
                    }
                }
            }),
            provider,
            _subscriptions: subscriptions,
        };
        this.sync(cx);
        this
    });
    cx.set_global(ClaudeCodeGlobal(state));
    #[cfg(debug_assertions)]
    dev::init(cx);
}

fn key() -> String {
    format!("{PLUGIN}/{TYPE_ID}")
}

impl ClaudeCode {
    fn global(cx: &App) -> Option<Entity<ClaudeCode>> {
        cx.try_global::<ClaudeCodeGlobal>()
            .map(|global| global.0.clone())
    }

    /// One spare session waits for whichever running agent needs it next.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(agents) = Agents2::global(cx) else {
            return;
        };
        // Turning the plugin off stops every session, as unloading it does
        // in buzz-app.
        if !crate::plugins::enabled(PLUGIN) {
            let _runtime = Runtime::handle(cx).enter();
            self.runtime.dispose();
            return;
        }
        let agents = agents.read(cx);
        // Agents that stop running, the app signing out included, stop
        // their processes; switching communities does not.
        if *agents.status() == Status::Loading {
            return;
        }
        let key = key();
        let defaults = &self.provider.read(cx).defaults;
        let synced: Vec<Synced> = agents
            .running()
            .iter()
            .filter(|agent| agent.agent_type == key)
            .filter_map(|agent| {
                let host = agents.host_of(&agent.pubkey)?;
                Some(Synced {
                    pubkey: agent.pubkey.clone(),
                    name: agent.name.clone(),
                    config: defaults.resolve(&agent.config),
                    spawn: host.read(cx).spawner(PROCESS_OWNER),
                    attention: agent.attention_enabled,
                })
            })
            .collect();
        let relays = owner_relay::running(agents, cx)
            .into_iter()
            .map(|(pubkey, (_, relay))| (pubkey, relay))
            .collect();
        self.copies.sync(agents, &key, cx);
        let _runtime = Runtime::handle(cx).enter();
        self.runtime.set_relays(relays);
        self.runtime.sync(synced);
    }
}

struct ClaudeCodeType;

/// The Claude tab before the runtime started, which `init` makes first.
struct Unavailable;

impl Render for Unavailable {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child("Claude agents run only in the desktop app")
    }
}

impl AgentType for ClaudeCodeType {
    fn id(&self) -> &'static str {
        TYPE_ID
    }

    fn title(&self) -> SharedString {
        "Claude".into()
    }

    fn description(&self) -> Option<SharedString> {
        Some(
            "Claude on this computer. Each thread it is mentioned in is one continuing session."
                .into(),
        )
    }

    fn mark(&self) -> Option<buzz_ui::BrandMark> {
        Some(buzz_ui::BrandMark::Claude)
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
        let provider = ClaudeCode::global(cx)?.read(cx).provider.clone();
        provider.update(cx, |provider, cx| provider.ensure_catalog(cx));
        Some(provider.read(cx).create_models())
    }

    fn provider(&self, cx: &App) -> Option<std::rc::Rc<dyn crate::agents2::AgentProvider>> {
        let provider = ClaudeCode::global(cx)?.read(cx).provider.clone();
        Some(std::rc::Rc::new(provider::Provider(provider)))
    }

    fn adopts(&self) -> bool {
        true
    }

    fn adopt(&self, carry: &buzz_host::api::Carry, saved: serde_json::Value) -> serde_json::Value {
        let mut config = Config::read(&saved);
        if let Some(instructions) = &carry.instructions {
            config.instructions = instructions.clone();
        }
        if let Some(workspace) = &carry.workspace {
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
                id: "sessions",
                label: "Sessions",
                is_override: config.scope != Config::default().scope,
                unchanged_key: Some(config.scope),
            },
        ]
    }

    fn summary(&self, agent: &Agent) -> Option<String> {
        let model = Config::read(&agent.config).model;
        Some(if model.is_empty() {
            "Claude".into()
        } else {
            model
        })
    }

    fn tabs(&self) -> Vec<AgentTab> {
        vec![
            AgentTab {
                id: "claude".into(),
                title: "Claude".into(),
                build: Arc::new(|view, window, cx| {
                    let Some(state) = ClaudeCode::global(cx) else {
                        return cx.new(|_| Unavailable).into();
                    };
                    let (pubkey, conversation) = (view.agent.pubkey.clone(), view.conversation);
                    let (setup, runtime) = state.update(cx, |state, cx| {
                        (state.copies.setup_of(&pubkey, cx), state.runtime.clone())
                    });
                    cx.new(|cx| {
                        tabs::ClaudeTab::new(pubkey, conversation, setup, runtime, window, cx)
                    })
                    .into()
                }),
            },
            AgentTab {
                id: "settings".into(),
                title: "Settings".into(),
                build: Arc::new(|view, window, cx| {
                    cx.new(|cx| tabs::SettingsTab::new(view, window, cx)).into()
                }),
            },
        ]
    }

    fn runs(&self) -> bool {
        true
    }

    fn models(&self, agent: &Agent, cx: &mut App) -> Option<Task<Result<Models, String>>> {
        let state = ClaudeCode::global(cx)?;
        let runtime = state.read(cx).runtime.clone();
        let pubkey = agent.pubkey.clone();
        let listed = Runtime::handle(cx).spawn(async move { runtime.models(&pubkey).await });
        Some(cx.background_spawn(async move {
            listed
                .await
                .unwrap_or_else(|_| Err("Claude stopped".to_string()))
        }))
    }

    fn run(&self, delivery: Delivery, cx: &mut App) -> Task<Result<(), String>> {
        tracing::debug!(agent = %delivery.agent.pubkey, "Claude delivery");
        let Some(state) = ClaudeCode::global(cx) else {
            return Task::ready(Err("Claude is not running".into()));
        };
        let host =
            Agents2::global(cx).and_then(|agents| agents.read(cx).host_of(&delivery.agent.pubkey));
        let Some(host) = host else {
            return Task::ready(Err("The agent's host is not connected".into()));
        };
        let spawn: Spawner = host.read(cx).spawner(PROCESS_OWNER);
        let activity = super::activity(&delivery);
        let setup = state.update(cx, |state, cx| state.copies.setup(&host, cx));
        let cancel = delivery.cancel.clone();
        let wake = match delivery.trigger {
            Trigger::Mention { event } => Wake::Mention {
                event: super::event_of(&event),
            },
            Trigger::Watch {
                event,
                slug,
                watch,
                classifier,
                ..
            } => Wake::Watch {
                event: super::event_of(&event),
                slug,
                interest: watch.interest_id,
                classifier: classifier.map(|skip| skip.reason),
            },
            Trigger::Timer {
                slug, timer, spent, ..
            } => Wake::Timer {
                slug,
                interest: timer.interest_id,
                spent,
            },
        };
        // A blank model inherits the device default.
        let config = state
            .read(cx)
            .provider
            .read(cx)
            .defaults
            .resolve(&delivery.config);
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
            current: delivery.current,
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
                .unwrap_or_else(|_| Err("Claude stopped".to_string()))
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
    fn a_brought_over_agent_keeps_its_instructions_and_workspace() {
        let saved = Config::default().value();
        let carry = Carry {
            instructions: Some("Look around.".into()),
            workspace: Some("~/code".into()),
        };
        let adopted = Config::read(&ClaudeCodeType.adopt(&carry, saved.clone()));
        assert_eq!(
            (adopted.instructions.as_str(), adopted.workspace.as_str()),
            ("Look around.", "~/code")
        );
        let kept = Config::read(&ClaudeCodeType.adopt(&Carry::default(), saved));
        assert_eq!(kept, Config::default(), "nothing carried, nothing changed");
    }

    #[test]
    fn carries_instructions_and_workspace_and_describes_its_overrides() {
        let saved = Config::default().value();
        assert_eq!(ClaudeCodeType.carry(&saved), Carry::default());
        let set = Config {
            model: "opus".into(),
            instructions: "Triage.".into(),
            workspace: "~/code".into(),
            scope: "channel".into(),
        };
        assert_eq!(
            ClaudeCodeType.carry(&set.value()),
            Carry {
                instructions: Some("Triage.".into()),
                workspace: Some("~/code".into()),
            }
        );
        let overrides = |config: &serde_json::Value| {
            ClaudeCodeType
                .describe_migration(config)
                .into_iter()
                .map(|setting| (setting.id, setting.is_override, setting.unchanged_key))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            overrides(&saved),
            [
                ("model", false, None),
                ("sessions", false, Some("thread".into()))
            ]
        );
        assert_eq!(
            overrides(&set.value()),
            [
                ("model", true, None),
                ("sessions", true, Some("channel".into()))
            ]
        );
        assert_eq!(ClaudeCodeType.migration_warning(), None);
    }
}
