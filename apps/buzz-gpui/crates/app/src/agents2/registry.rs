//! The agent-type registry (buzz-app's `agents2.register`).
//!
//! An agent type is compiled in: it implements [`AgentType`] and registers
//! itself once at startup, from its module's `init`, with
//! [`register_type`]:
//!
//! ```ignore
//! pub fn init(cx: &mut App) {
//!     agents2::register_type("buzz.claude-code", Arc::new(ClaudeCode), cx)
//!         .expect("a valid agent type");
//! }
//! ```
//!
//! Its key, `<plugin>/<id>`, is saved with every agent made from it and
//! never changes. The app owns each agent's name, key and attention; the
//! type owns its config (one JSON value) and decides what a run does.

// The type API: agent types compiled in elsewhere use what the app does not.
#![allow(dead_code)]

use super::Agent;
use super::attention::AttentionValue;
use super::runtime::Delivery;
use gpui_kit::{AnyElement, AnyView, App, Global, SharedString, Task, Window};
use serde_json::Value;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// The longest a run may take.
pub const TIMEOUT_LIMIT: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// A new agent's starting settings.
#[derive(Clone, Debug, Default)]
pub struct TypeDefaults {
    pub config: Value,
    /// Attention objects by slug, such as an Interest and its watches.
    pub attention: BTreeMap<String, AttentionValue>,
}

/// What a type's settings tab is built from. A type writes only its own
/// config; the app owns the name and attention.
#[derive(Clone)]
pub struct AgentView {
    pub agent: Agent,
    /// Where the agent was opened from, when it was opened from a
    /// conversation.
    pub conversation: Option<AgentConversation>,
    /// The Agents surfaces' framing for the type's Settings tab; other tabs
    /// and surfaces have none.
    pub settings_host: Option<SettingsHost>,
}

/// The conversation of the message a profile was opened from: its
/// channel, and the message's thread root (the message itself when it is
/// not a reply). Which session that is stays the type's own rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentConversation {
    pub channel_id: String,
    pub root_id: Option<String>,
}

impl AgentView {
    /// Replaces the type's whole config. Change one field by editing a copy
    /// of `agent.config`, or tabs that each save a part erase each other's
    /// fields. With a provider change staged, saves the change and this
    /// config together.
    pub fn save(&self, config: Value, cx: &mut App) -> Task<Result<(), String>> {
        match self
            .settings_host
            .as_ref()
            .and_then(|host| host.pending.clone())
        {
            Some(pending) => pending(config, cx),
            None => super::Agents2::save_config(&self.agent.pubkey, config, cx),
        }
    }

    /// The staged provider change's settings, which the tab starts from in
    /// place of the saved ones.
    pub fn staged(&self) -> Option<&Value> {
        self.settings_host
            .as_ref()
            .filter(|host| host.pending.is_some())
            .map(|_| &self.agent.config)
    }

    /// Whether Save has something to commit even without edits: a staged
    /// provider change.
    pub fn pending(&self) -> bool {
        self.staged().is_some()
    }

    /// The Provider field at the top of Settings, told whether the tab has
    /// unsaved changes; `None` without another provider to move to.
    pub fn type_control(
        &self,
        dirty: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let control = self.settings_host.as_ref()?.type_control.clone()?;
        Some(control(dirty, window, cx))
    }
}

/// Renders the Provider field, given whether Settings has unsaved changes.
pub type TypeControl = Rc<dyn Fn(bool, &mut Window, &mut App) -> AnyElement>;
/// Commits a staged provider change with the destination's settings.
pub type PendingSave = Rc<dyn Fn(Value, &mut App) -> Task<Result<(), String>>>;

/// How the Agents surfaces frame a type's Settings tab
/// (`AgentSettingsHost`): the provider select, and a staged change of
/// provider that Save commits.
#[derive(Clone, Default)]
pub struct SettingsHost {
    pub type_control: Option<TypeControl>,
    /// Set while a provider change is staged; the view's agent then has
    /// the destination type and its staged settings.
    pub pending: Option<PendingSave>,
}

/// A hosted type's provider (`RemoteAgentType`): it holds each agent's
/// key and runs it. The app calls these in order to make one
/// (`provision`, then `activate`) and owns the owner's agent list; each
/// step may be asked again after a failure, so each must be safe to
/// repeat. `community` is the agent's `wss://` community.
pub trait RemoteAgentType {
    /// Makes the agent and resolves with its pubkey, not yet the owner's.
    fn provision(&self, name: &str, community: &str, cx: &mut App) -> Task<Result<String, String>>;
    /// Hands over the owner's NIP-OA attestation (`["auth", owner, "",
    /// signature]`); the provider publishes its profile and starts it.
    fn activate(
        &self,
        pubkey: &str,
        name: &str,
        community: &str,
        auth: Vec<String>,
        cx: &mut App,
    ) -> Task<Result<(), String>>;
    /// Publishes its profile with this name and picture.
    fn profile(
        &self,
        pubkey: &str,
        community: &str,
        name: &str,
        picture: Option<&str>,
        cx: &mut App,
    ) -> Task<Result<(), String>>;
    /// Stops it and deletes its key; one already gone counts as removed.
    fn remove(&self, pubkey: &str, community: &str, cx: &mut App) -> Task<Result<(), String>>;
}

/// Gives the New agent dialog a new starting config from a type's form.
pub type CreateChange = Rc<dyn Fn(Value, &mut App)>;

/// Builds a tab's view. Called when the tab is shown; the view may read
/// the current agent from [`super::Agents2`] to follow later changes.
pub type BuildTab = Arc<dyn Fn(AgentView, &mut Window, &mut App) -> AnyView>;

/// One of a type's settings tabs, shown before the app's Attention tab.
#[derive(Clone)]
pub struct AgentTab {
    pub id: SharedString,
    pub title: SharedString,
    pub build: BuildTab,
}

/// An effort level a model offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effort {
    pub id: String,
    pub label: String,
}

/// A model with its effort levels, lowest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub efforts: Vec<Effort>,
    /// The effort a run on it uses without a pick, when known.
    pub effort: Option<String>,
}

/// The models an owner can pick from, and the id of the one used without a
/// pick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Models {
    pub current: String,
    pub models: Vec<Model>,
}

/// One model a type offers (`ModelChoice`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoice {
    /// The model id saved in the type's config.
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// A type's model choices for a new agent (`ModelPicker` as a menu): its
/// default row first, then `models`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateModels {
    pub models: Vec<ModelChoice>,
    pub default_label: String,
    pub default_description: Option<String>,
    /// Whether an id absent from `models` may be entered.
    pub allow_custom: bool,
    /// The catalog is still loading.
    pub loading: bool,
}

/// One harness setting a provider-change preview names
/// (`AgentMigrationSetting`). Labels never decide what migrates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationSetting {
    pub id: &'static str,
    pub label: &'static str,
    /// The agent sets it rather than following the default.
    pub is_override: bool,
    /// Equal keys on both sides mean the override has the same effect
    /// after the change.
    pub unchanged_key: Option<String>,
}

/// What another type may adopt when an agent changes type (`Carry`'s keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarryKey {
    Instructions,
    Workspace,
}

impl CarryKey {
    pub const ALL: [CarryKey; 2] = [CarryKey::Instructions, CarryKey::Workspace];

    pub fn label(self) -> &'static str {
        match self {
            CarryKey::Instructions => "Instructions",
            CarryKey::Workspace => "Workspace",
        }
    }

    fn of(self, carry: &buzz_host::api::Carry) -> Option<&String> {
        match self {
            CarryKey::Instructions => carry.instructions.as_ref(),
            CarryKey::Workspace => carry.workspace.as_ref(),
        }
    }
}

/// What a provider change does to one of the old type's overrides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// The new type has the setting; it starts from its default.
    Reset,
    /// The new type has no such setting.
    Dropped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigratedSetting {
    pub id: &'static str,
    pub label: &'static str,
    pub outcome: MigrationOutcome,
}

/// What changing an agent's type would do (`RetypePreview`).
#[derive(Clone, Debug, PartialEq)]
pub struct RetypePreview {
    /// The destination's settings after its migration policy.
    pub config: Value,
    pub moved: Vec<CarryKey>,
    pub dropped: Vec<CarryKey>,
    pub settings: Vec<MigratedSetting>,
}

/// The one migration both the preview and the change use (`retypePlan`):
/// the destination's defaults with what of the source's carry it adopts.
/// `source` is `None` for a type no longer available, which carries
/// nothing.
pub fn retype_plan(
    source: Option<&dyn AgentType>,
    config: &Value,
    kind: &dyn AgentType,
) -> RetypePreview {
    let carry = source
        .map(|source| source.carry(config))
        .unwrap_or_default();
    let adopted = kind.adopt(&carry, kind.defaults().config);
    let kept = kind.carry(&adopted);
    let (mut moved, mut dropped) = (Vec::new(), Vec::new());
    for key in CarryKey::ALL {
        let Some(value) = key.of(&carry) else {
            continue;
        };
        if key.of(&kept) == Some(value) {
            moved.push(key);
        } else {
            dropped.push(key);
        }
    }
    let destination = kind.describe_migration(&adopted);
    let settings = source
        .map(|source| source.describe_migration(config))
        .unwrap_or_default()
        .into_iter()
        .filter(|setting| setting.is_override)
        .filter_map(|setting| {
            let next = destination.iter().find(|next| next.id == setting.id);
            if let Some(next) = next
                && setting.unchanged_key.is_some()
                && setting.unchanged_key == next.unchanged_key
            {
                return None;
            }
            Some(MigratedSetting {
                id: setting.id,
                label: setting.label,
                outcome: if next.is_some() {
                    MigrationOutcome::Reset
                } else {
                    MigrationOutcome::Dropped
                },
            })
        })
        .collect();
    RetypePreview {
        config: adopted,
        moved,
        dropped,
        settings,
    }
}

/// How a type's provider stands on this device (`AgentProviderSnapshot`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    Checking,
    Ready,
    Attention,
}

impl Readiness {
    /// The state as buzz-app names it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::Ready => "ready",
            Self::Attention => "attention",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSnapshot {
    pub readiness: Readiness,
    /// Action-oriented menu copy for direct recovery or recovery guidance.
    pub attention_label: Option<String>,
    /// Installation and sign-in work must be cancelled before its dialog
    /// closes.
    pub blocking: bool,
}

impl ProviderSnapshot {
    pub const CHECKING: Self = Self {
        readiness: Readiness::Checking,
        attention_label: None,
        blocking: false,
    };
}

/// Closes the provider dialog a body is shown in.
pub type CloseProvider = Rc<dyn Fn(&mut Window, &mut App)>;

/// Recovery a provider runs straight from its menu row, such as an install.
pub type AttentionAction = Box<dyn FnOnce(&mut App)>;

/// A type's device-wide readiness and management (`AgentProvider`). The
/// Agents page supplies the menu row, the type's mark and the dialog
/// framing. A provider calls [`providers_changed`] whenever its snapshot,
/// catalog or defaults change.
pub trait AgentProvider {
    fn snapshot(&self, cx: &App) -> ProviderSnapshot;
    /// Refreshes device-wide readiness without changing installation or
    /// sign-in.
    fn check(&self, cx: &mut App);
    /// Handles the current attention state directly; `None` opens the
    /// dialog.
    fn attention_action(&self, _cx: &App) -> Option<AttentionAction> {
        None
    }
    /// Recovery and progress controls, and optional saved defaults.
    fn dialog_body(&self, close: CloseProvider, window: &mut Window, cx: &mut App) -> AnyView;
}

/// Bumped on every provider change, for views to observe.
#[derive(Default)]
pub struct ProviderChanges(u64);

impl Global for ProviderChanges {}

/// Tells observers of [`ProviderChanges`] that a provider changed.
pub fn providers_changed(cx: &mut App) {
    cx.default_global::<ProviderChanges>().0 += 1;
}

/// A kind of Agents2 agent. Everything but `id`, `title` and `defaults`
/// has a default.
pub trait AgentType: 'static {
    /// Lowercase letters, digits, `.`, `_` and `-`, starting with a letter
    /// or digit. Unique within its plugin.
    fn id(&self) -> &'static str;
    fn title(&self) -> SharedString;
    /// Shown under the title in the New agent form.
    fn description(&self) -> Option<SharedString> {
        None
    }
    /// The type's mark, shown with it in the app's own choices; the robot
    /// without one.
    fn mark(&self) -> Option<buzz_ui::BrandMark> {
        None
    }
    /// A design-system icon for the type when it has no brand mark
    /// (`icon`); the robot without either.
    fn icon(&self) -> Option<buzz_ui::IconName> {
        None
    }
    /// A new agent's starting config and attention objects.
    fn defaults(&self) -> TypeDefaults;
    /// Whether the New agent dialog's card offers the agent's instructions
    /// (its config's `instructions`), as Claude's and Codex's `Create` do.
    fn create_instructions(&self) -> bool {
        false
    }
    /// The model rows of this type's submenu in the New agent dialog
    /// (`CreateModelMenu`); `None` offers no model choice. Their ids are
    /// saved as the config's `model`, through [`Self::with_model`].
    fn create_models(&self, _cx: &mut App) -> Option<CreateModels> {
        None
    }
    /// `config` with `model` chosen; a type resets what depends on it.
    fn with_model(&self, config: Value, model: &str, _cx: &App) -> Value {
        let mut config = config;
        if let Value::Object(fields) = &mut config {
            fields.insert("model".into(), Value::from(model));
        }
        config
    }
    /// The type's own part of the New agent dialog (`Create`), such as a
    /// plugin's form: it starts from `config` and reports each change
    /// through `change`. `None` keeps the dialog's own controls.
    fn create_form(
        &self,
        _config: &Value,
        _change: CreateChange,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<AnyView> {
        None
    }
    /// The non-default model a New agent's config names, for the type
    /// selector (`createModelLabel`).
    fn create_model_label(&self, config: &Value, cx: &mut App) -> Option<String> {
        let model = config.get("model")?.as_str()?.trim();
        if model.is_empty() {
            return None;
        }
        let models = self.create_models(cx);
        Some(
            models
                .and_then(|models| {
                    models
                        .models
                        .into_iter()
                        .find(|choice| choice.id == model)
                        .map(|choice| choice.label)
                })
                .unwrap_or_else(|| model.to_owned()),
        )
    }
    /// Device-wide readiness and management, under Provider settings on
    /// the Agents page; `None` lists the type there as unavailable.
    fn provider(&self, _cx: &App) -> Option<Rc<dyn AgentProvider>> {
        None
    }
    /// One short line under the agent's name, such as its model. `None` or
    /// blank shows the title.
    fn summary(&self, _agent: &Agent) -> Option<String> {
        None
    }
    /// A read-only glance shown when the agent is selected in the grid.
    fn peek(&self, _agent: &Agent, _window: &mut Window, _cx: &mut App) -> Option<AnyElement> {
        None
    }
    /// The type's settings tabs, in order. The app adds Attention after
    /// them.
    fn tabs(&self) -> Vec<AgentTab> {
        Vec::new()
    }
    /// Makes the type hosted: its provider runs each agent, so it has no
    /// run, models or timeout, and its agents have no attention.
    fn remote(&self) -> Option<Rc<dyn RemoteAgentType>> {
        None
    }
    /// Whether [`Self::run`] does anything. Agents of a type that does not
    /// run drop what wakes them.
    fn runs(&self) -> bool {
        false
    }
    /// Models and effort levels the owner can pick for the next run while
    /// mentioning the agent. Discovery may take a while but must not start
    /// a turn. The picks reach that run as [`Delivery::model`] and
    /// [`Delivery::effort`]. `None` offers no choice.
    fn models(&self, _agent: &Agent, _cx: &mut App) -> Option<Task<Result<Models, String>>> {
        None
    }
    /// Handles one wake. Runs of one agent are serialized: the next starts
    /// when this task ends or its deadline passes, whichever is first.
    fn run(&self, _delivery: Delivery, _cx: &mut App) -> Task<Result<(), String>> {
        Task::ready(Ok(()))
    }
    /// The per-run deadline, from 1 ms to 30 minutes.
    fn timeout(&self) -> Duration {
        DEFAULT_TIMEOUT
    }
    /// Whether [`Self::adopt`] gives a brought-over agent anything it kept
    /// in an earlier agent system.
    fn adopts(&self) -> bool {
        false
    }
    /// A brought-over agent's config: `config`, the type's defaults, with
    /// what of `carry` the type has a place for.
    fn adopt(&self, _carry: &buzz_host::api::Carry, config: Value) -> Value {
        config
    }
    /// What of `config` another type may adopt when the agent changes
    /// type. Without it, all of its settings stay behind.
    fn carry(&self, _config: &Value) -> buzz_host::api::Carry {
        buzz_host::api::Carry::default()
    }
    /// The harness settings a provider-change preview describes.
    fn describe_migration(&self, _config: &Value) -> Vec<MigrationSetting> {
        Vec::new()
    }
    /// A brief consequence of leaving this type, for the preview.
    fn migration_warning(&self) -> Option<&'static str> {
        None
    }
}

/// A registered type and its key.
#[derive(Clone)]
pub struct RegisteredType {
    /// `<plugin>/<id>`, saved with each agent.
    pub key: SharedString,
    pub kind: Arc<dyn AgentType>,
}

impl RegisteredType {
    /// The type's summary of `agent`; its title when it has none.
    pub fn summary_of(&self, agent: &Agent) -> String {
        self.kind
            .summary(agent)
            .map(|summary| summary.trim().to_owned())
            .filter(|summary| !summary.is_empty())
            .unwrap_or_else(|| self.kind.title().to_string())
    }
}

/// The registered types, in registration order.
#[derive(Default)]
pub struct AgentTypes(pub Vec<RegisteredType>);

impl Global for AgentTypes {}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// Checks a type before it is registered.
pub fn validate(plugin: &str, kind: &dyn AgentType) -> Result<(), String> {
    if !valid_name(plugin) || !valid_name(kind.id()) || kind.title().is_empty() {
        return Err("Agent types need an id, a title and defaults".into());
    }
    let timeout = kind.timeout();
    if timeout < Duration::from_millis(1) || timeout > TIMEOUT_LIMIT {
        return Err(format!(
            "Agent type timeoutMs must be a whole number from 1 to {}",
            TIMEOUT_LIMIT.as_millis()
        ));
    }
    Ok(())
}

/// Registers `kind` as `<plugin>/<id>`, replacing a type registered under
/// the same key. Agents of a replaced type abort their run in flight and
/// continue with the new one.
pub fn register_type(plugin: &str, kind: Arc<dyn AgentType>, cx: &mut App) -> Result<(), String> {
    validate(plugin, kind.as_ref())?;
    let key: SharedString = format!("{plugin}/{}", kind.id()).into();
    let types = &mut cx.default_global::<AgentTypes>().0;
    types.retain(|existing| existing.key != key);
    types.push(RegisteredType { key, kind });
    Ok(())
}

/// Removes the type `key`, as when the plugin that added it stops. Its
/// agents show their type as unavailable until it is back.
pub fn unregister_type(key: &str, cx: &mut App) {
    cx.default_global::<AgentTypes>()
        .0
        .retain(|existing| existing.key != key);
}

/// Every registered type whose plugin is on. A plugin turned off takes its
/// types with it, as unloading it does in buzz-app: its agents show their
/// type as unavailable and stop running until it is back.
pub fn types(cx: &App) -> Vec<RegisteredType> {
    cx.try_global::<AgentTypes>()
        .map(|types| {
            types
                .0
                .iter()
                .filter(|registered| crate::plugins::enabled_key(&registered.key))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The registered type with this key, while its plugin is on.
pub fn type_of(key: &str, cx: &App) -> Option<RegisteredType> {
    cx.try_global::<AgentTypes>()?
        .0
        .iter()
        .find(|registered| registered.key == key)
        .filter(|registered| crate::plugins::enabled_key(&registered.key))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_host::api::Carry;
    use core::prelude::v1::test;
    use serde_json::json;

    /// buzz-app's service test's Echo: it carries its reply and a
    /// workspace, and overrides everything.
    struct Echo;
    impl AgentType for Echo {
        fn id(&self) -> &'static str {
            "echo"
        }
        fn title(&self) -> SharedString {
            "Echo".into()
        }
        fn defaults(&self) -> TypeDefaults {
            TypeDefaults {
                config: json!({ "reply": "ok" }),
                ..Default::default()
            }
        }
        fn carry(&self, config: &Value) -> Carry {
            Carry {
                instructions: config["reply"].as_str().map(str::to_owned),
                workspace: Some("~/echo".into()),
            }
        }
        fn describe_migration(&self, _: &Value) -> Vec<MigrationSetting> {
            let setting = |id, label, unchanged_key: Option<&str>| MigrationSetting {
                id,
                label,
                is_override: true,
                unchanged_key: unchanged_key.map(str::to_owned),
            };
            vec![
                setting("model", "Model", None),
                setting("thinking", "Thinking", None),
                setting("sessions", "Sessions", Some("channel")),
                setting("equivalent", "Equivalent setting", Some("same")),
                MigrationSetting {
                    is_override: false,
                    ..setting("default", "Default setting", None)
                },
            ]
        }
    }

    /// Its Relay: it adopts instructions as its prompt and has no
    /// workspace.
    struct Relay;
    impl AgentType for Relay {
        fn id(&self) -> &'static str {
            "relay"
        }
        fn title(&self) -> SharedString {
            "Relay".into()
        }
        fn defaults(&self) -> TypeDefaults {
            TypeDefaults {
                config: json!({ "prompt": "", "model": "small" }),
                ..Default::default()
            }
        }
        fn adopt(&self, carry: &Carry, mut config: Value) -> Value {
            if let Some(instructions) = &carry.instructions {
                config["prompt"] = json!(instructions);
            }
            config
        }
        fn carry(&self, config: &Value) -> Carry {
            Carry {
                instructions: config["prompt"].as_str().map(str::to_owned),
                workspace: None,
            }
        }
        fn describe_migration(&self, _: &Value) -> Vec<MigrationSetting> {
            let setting = |id, label, unchanged_key: Option<&str>| MigrationSetting {
                id,
                label,
                is_override: false,
                unchanged_key: unchanged_key.map(str::to_owned),
            };
            vec![
                setting("model", "Model", None),
                setting("sessions", "Sessions", Some("thread")),
                setting("equivalent", "Equivalent setting", Some("same")),
                setting("destination", "Destination only", None),
            ]
        }
    }

    #[test]
    fn previews_what_carries_resets_and_drops() {
        let preview = retype_plan(Some(&Echo), &json!({ "reply": "Triage." }), &Relay);
        let setting = |id, label, outcome| MigratedSetting { id, label, outcome };
        assert_eq!(
            preview,
            RetypePreview {
                config: json!({ "prompt": "Triage.", "model": "small" }),
                moved: vec![CarryKey::Instructions],
                dropped: vec![CarryKey::Workspace],
                settings: vec![
                    setting("model", "Model", MigrationOutcome::Reset),
                    setting("thinking", "Thinking", MigrationOutcome::Dropped),
                    setting("sessions", "Sessions", MigrationOutcome::Reset),
                ],
            }
        );
    }

    #[test]
    fn an_unavailable_type_carries_nothing() {
        let preview = retype_plan(None, &json!({ "reply": "Triage." }), &Relay);
        assert_eq!(preview.config, json!({ "prompt": "", "model": "small" }));
        assert!(preview.moved.is_empty() && preview.dropped.is_empty());
        assert!(preview.settings.is_empty());
    }
}
