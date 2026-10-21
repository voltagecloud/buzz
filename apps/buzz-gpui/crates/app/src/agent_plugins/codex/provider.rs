//! Codex as a provider on this computer (buzz-app's `CodexProvider` and
//! `ProviderDialogBody`): one device-wide check of the account and models
//! on the embedded host, and the default model and thinking agents
//! inherit.

use super::config::{Config, Model, effort_name};
use super::defaults::{
    CodexDefaults, CodexProviderDefaults, default_effort_label, find, native_label,
    provider_default_label,
};
use super::setup::{CodexSetup, CodexStatus, SetupState};
use super::tabs::{Catalog, NOT_INSTALLED, SIGN_IN, catalog, setup_action, setup_steps};
use crate::agent_plugins::process::Spawner;
use crate::agent_plugins::provider::{Choice, described_choice};
use crate::agent_plugins::tabs::{Picking, field};
use crate::agents2::{
    AgentProvider, CloseProvider, CreateModels, ModelChoice, ProviderSnapshot, Readiness,
    providers_changed,
};
use crate::buzz_mcp::app_client::Signal;
use crate::runtime::Runtime;
use buzz_ui::{ActiveTheme, Button, TypeStyled, tokens::space};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::Value;

/// Codex's one device-wide health and catalog view, and its defaults.
pub struct CodexProvider {
    spawn: Option<Spawner>,
    /// Buzz's copy of Codex on the embedded host, and its install or
    /// sign-in as it runs.
    setup: CodexSetup,
    setup_state: SetupState,
    pub snapshot: ProviderSnapshot,
    pub checking: bool,
    pub data: Option<Catalog>,
    pub error: String,
    pub defaults: CodexDefaults,
    signal: Signal,
    _task: Option<Task<()>>,
    _follow: Task<()>,
}

impl CodexProvider {
    pub fn new(spawn: Option<Spawner>, setup: CodexSetup, cx: &mut Context<Self>) -> Self {
        // An install or sign-in holds the dialog open, then Codex is checked
        // again.
        let mut changes = setup.subscribe();
        let follow = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                let state = changes.borrow_and_update().clone();
                let alive = this.update(cx, |this, cx| {
                    let running = state.running.is_some();
                    let was = this.setup_state.running.is_some();
                    this.setup_state = state;
                    if running != was {
                        this.snapshot.blocking = running;
                        providers_changed(cx);
                        if !running {
                            this.check(cx);
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        Self {
            spawn,
            setup_state: setup.snapshot(),
            setup,
            snapshot: ProviderSnapshot::CHECKING,
            checking: false,
            data: None,
            error: String::new(),
            defaults: CodexDefaults::load(),
            signal: Signal::default(),
            _task: None,
            _follow: follow,
        }
    }

    fn models(&self) -> Option<&[Model]> {
        self.data.as_ref().map(|data| data.models.as_slice())
    }

    /// Checks the account and models; a check in flight is the one.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        if self.checking {
            return;
        }
        // A re-check keeps Codex ready until it answers otherwise.
        if self.data.is_none() {
            self.snapshot = ProviderSnapshot::CHECKING;
        }
        self.checking = true;
        self.error.clear();
        self.signal = Signal::default();
        let signal = self.signal.clone();
        let setup = self.setup.clone();
        let check = match self.spawn.clone() {
            Some(spawn) => Runtime::handle(cx).spawn({
                let signal = signal.clone();
                async move {
                    match setup.check().await {
                        CodexStatus::Ready { .. } => catalog(spawn, signal).await,
                        CodexStatus::Missing => Err(NOT_INSTALLED.into()),
                        CodexStatus::SignedOut { .. } | CodexStatus::Outdated { .. } => {
                            Err(SIGN_IN.into())
                        }
                        CodexStatus::Error { message } => Err(message),
                    }
                }
            }),
            None => Runtime::handle(cx).spawn(async {
                Err::<Catalog, _>("Codex agents run only in the desktop app".to_string())
            }),
        };
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = check
                .await
                .unwrap_or_else(|_| Err("Codex check stopped".into()));
            let _ = this.update(cx, |this, cx| {
                if signal.aborted() {
                    return;
                }
                this.checking = false;
                let blocking = this.setup_state.running.is_some();
                match result {
                    Ok(data) => {
                        this.snapshot = ProviderSnapshot {
                            readiness: Readiness::Ready,
                            attention_label: None,
                            blocking,
                        };
                        this.data = Some(data);
                    }
                    Err(error) => {
                        let label = match error.as_str() {
                            NOT_INSTALLED => "Install Codex",
                            SIGN_IN => "Log in to Codex",
                            _ => "Codex needs attention",
                        };
                        this.snapshot = ProviderSnapshot {
                            readiness: Readiness::Attention,
                            attention_label: Some(label.into()),
                            blocking,
                        };
                        this.data = None;
                        this.error = error;
                    }
                }
                cx.notify();
                providers_changed(cx);
            });
        }));
        cx.notify();
        providers_changed(cx);
    }

    /// A surface showing the catalog checks once, as `useProvider` does.
    pub fn ensure_checked(&mut self, cx: &mut Context<Self>) {
        if self.snapshot.readiness == Readiness::Checking && !self.checking {
            self.check(cx);
        }
    }

    pub fn save_defaults(
        &mut self,
        next: CodexProviderDefaults,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let models = self.data.as_ref().map(|data| data.models.clone());
        self.defaults.save(next, models.as_deref())?;
        cx.notify();
        providers_changed(cx);
        Ok(())
    }

    /// The agent's saved config with blank choices inheriting the device
    /// defaults.
    pub fn resolve(&self, raw: &Value) -> Value {
        let resolved = self.defaults.resolve(Config::read(raw), self.models());
        let mut value = raw.clone();
        match &mut value {
            Value::Object(fields) => {
                fields.insert("model".into(), Value::from(resolved.model));
                fields.insert("effort".into(), Value::from(resolved.effort));
            }
            _ => value = resolved.value(),
        }
        value
    }

    /// Choosing a model also resets the effort to that model's default.
    pub fn with_model(&self, raw: Value, model: &str) -> Value {
        let mut config = Config::read(&raw);
        config.effort = self
            .models()
            .and_then(|models| models.iter().find(|candidate| candidate.model == model))
            .map(|model| model.default_reasoning_effort.clone())
            .unwrap_or_default();
        config.model = model.to_owned();
        let mut value = raw;
        match &mut value {
            Value::Object(fields) => {
                fields.insert("model".into(), Value::from(config.model));
                fields.insert("effort".into(), Value::from(config.effort));
            }
            _ => value = config.value(),
        }
        value
    }

    /// Codex's rows in the New agent dialog's model menu.
    pub fn create_models(&self) -> CreateModels {
        CreateModels {
            models: self
                .models()
                .unwrap_or_default()
                .iter()
                .map(|model| ModelChoice {
                    id: model.model.clone(),
                    label: model.display_name.clone(),
                    description: (!model.description.is_empty()).then(|| model.description.clone()),
                })
                .collect(),
            default_label: provider_default_label(&self.defaults.snapshot().model, self.models()),
            default_description: Some("Following the provider's model default".into()),
            allow_custom: false,
            loading: self.checking,
        }
    }
}

impl Drop for CodexProvider {
    fn drop(&mut self) {
        self.signal.abort("Codex check closed");
    }
}

/// The default row of an agent's model choice: the device default when
/// one is saved and listed, else Codex's own (`providerDefaultLabel`).
pub fn default_model_label(cx: &App) -> String {
    match super::Codex::global(cx) {
        Some(state) => {
            state
                .read(cx)
                .provider
                .read(cx)
                .create_models()
                .default_label
        }
        None => native_label(None),
    }
}

/// The default row of an agent's thinking choice while its draft names
/// `model`: the effort it would run at with the device defaults.
pub fn default_effort_label_for(model: &str, cx: &App) -> String {
    let Some(state) = super::Codex::global(cx) else {
        return default_effort_label("", None);
    };
    let provider = state.read(cx).provider.read(cx);
    let draft = Config {
        model: model.to_owned(),
        effort: String::new(),
        ..Config::default()
    };
    let effective = provider.defaults.resolve(draft, provider.models());
    default_effort_label(&effective.effort, find(provider.models(), &effective.model))
}

/// The type's provider, for the Agents page.
pub struct Provider(pub Entity<CodexProvider>);

impl AgentProvider for Provider {
    fn snapshot(&self, cx: &App) -> ProviderSnapshot {
        self.0.read(cx).snapshot.clone()
    }

    fn check(&self, cx: &mut App) {
        self.0.update(cx, |provider, cx| provider.check(cx));
    }

    fn dialog_body(&self, close: CloseProvider, _: &mut Window, cx: &mut App) -> AnyView {
        let provider = self.0.clone();
        cx.new(|cx| ProviderBody::new(provider, close, cx)).into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picker {
    Model,
    Effort,
}

/// The provider dialog's body: Codex's state until it is ready, then the
/// device-wide default model and thinking.
pub struct ProviderBody {
    provider: Entity<CodexProvider>,
    draft: CodexProviderDefaults,
    /// The saved defaults the draft started from.
    saved: CodexProviderDefaults,
    open: Option<Picker>,
    error: Option<String>,
    close: CloseProvider,
    _subscription: Subscription,
}

impl ProviderBody {
    fn new(provider: Entity<CodexProvider>, close: CloseProvider, cx: &mut Context<Self>) -> Self {
        let saved = provider.read(cx).defaults.snapshot().clone();
        // Saved defaults that change replace the draft.
        let subscription = cx.observe(&provider, |this, provider, cx| {
            let saved = provider.read(cx).defaults.snapshot().clone();
            if saved != this.saved {
                this.draft = saved.clone();
                this.saved = saved;
            }
            cx.notify();
        });
        provider.update(cx, |provider, cx| provider.ensure_checked(cx));
        Self {
            provider,
            draft: saved.clone(),
            saved,
            open: None,
            error: None,
            close,
            _subscription: subscription,
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.draft.clone();
        match self
            .provider
            .update(cx, |provider, cx| provider.save_defaults(next, cx))
        {
            Ok(()) => {
                self.error = None;
                (self.close)(window, cx);
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }
}

impl Picking for ProviderBody {
    type Picker = Picker;

    fn open(&mut self) -> &mut Option<Picker> {
        &mut self.open
    }

    fn pick(&mut self, picker: Picker, value: String, _: &mut Context<Self>) {
        match picker {
            Picker::Model => {
                self.draft.model = value;
                self.draft.effort.clear();
            }
            Picker::Effort => self.draft.effort = value,
        }
    }
}

impl Render for ProviderBody {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let provider = self.provider.read(cx);
        let ready = provider.snapshot.readiness == Readiness::Ready;
        let checking = provider.checking;
        let error = provider.error.clone();
        let installing = provider.setup_state.clone();
        let setup = setup_steps(
            &provider.setup,
            &installing,
            setup_action(&error),
            checking,
            &c,
        );
        let models = provider.models().map(<[Model]>::to_vec);
        let models = models.as_deref();
        let native = find(models, "");
        let selected = find(models, &self.draft.model);
        let mut model_choices = vec![
            Choice::new("", native_label(models))
                .described(native.map(|_| "Following Codex's recommendation".to_string())),
        ];
        if !self.draft.model.is_empty() && selected.is_none() {
            model_choices.push(Choice::new(
                self.draft.model.clone(),
                format!("{} (unavailable)", self.draft.model),
            ));
        }
        model_choices.extend(models.unwrap_or_default().iter().map(|model| {
            Choice::new(model.model.clone(), model.display_name.clone())
                .described(Some(model.description.clone()))
        }));
        let mut efforts = vec![Choice::new("", default_effort_label("", selected))];
        let offered = |effort: &str| {
            selected.is_some_and(|model| {
                model
                    .supported_reasoning_efforts
                    .iter()
                    .any(|choice| choice.reasoning_effort == effort)
            })
        };
        if !self.draft.effort.is_empty() && !offered(&self.draft.effort) {
            efforts.push(Choice::new(
                self.draft.effort.clone(),
                format!("{} (unavailable)", effort_name(&self.draft.effort)),
            ));
        }
        efforts.extend(selected.into_iter().flat_map(|model| {
            model
                .supported_reasoning_efforts
                .iter()
                .map(|effort| {
                    Choice::new(
                        effort.reasoning_effort.clone(),
                        effort_name(&effort.reasoning_effort),
                    )
                })
                .collect::<Vec<_>>()
        }));
        let no_selection = selected.is_none();
        let no_data = models.is_none();
        let draft = self.draft.clone();
        let changed = self.draft != self.saved;
        let status = div()
            .flex()
            .flex_col()
            .gap(space::S2)
            .map(|this| {
                if installing.running.is_some() {
                    this
                } else if checking {
                    this.child(div().child("Checking Codex…"))
                } else if !error.is_empty() {
                    this.child(
                        div()
                            .text_body_sm()
                            .font_weight(FontWeight::BOLD)
                            .child(error.clone()),
                    )
                } else {
                    this
                }
            })
            .child(setup);
        let model_select = described_choice(
            self.open,
            Picker::Model,
            "codex-default-model",
            &draft.model,
            model_choices,
            checking || no_data,
            window,
            cx,
        );
        let effort_select = described_choice(
            self.open,
            Picker::Effort,
            "codex-default-effort",
            &draft.effort,
            efforts,
            checking || no_selection,
            window,
            cx,
        );
        let form = div()
            .flex()
            .flex_col()
            .gap(space::S3)
            .child(field("Default model", model_select, None, &c))
            .child(field("Default thinking", effort_select, None, &c))
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            )
            .child(
                div().flex().child(
                    Button::new("codex-save-defaults", "Save defaults")
                        .prominent()
                        .disabled(!changed)
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                ),
            );
        div()
            .flex()
            .flex_col()
            .gap(space::S6)
            .text_body()
            .children(
                installing
                    .notice
                    .map(|notice| div().text_body_sm().child(notice)),
            )
            .when(!ready, |this| this.child(status))
            .when(ready, |this| this.child(form))
    }
}
