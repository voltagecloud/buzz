//! Claude as a provider on this computer (buzz-app's `ClaudeSetup`
//! provider snapshot, `loadModels` and `ProviderDialogBody`): its readiness
//! on the embedded host, the models it lists, and the device-wide default
//! model. [`SetupControls`], the install and sign-in controls, are shared
//! with the Claude tab.

use super::claude::{ClaudeModel, ClaudeProcess, Launch};
use super::defaults::{ClaudeDefaults, ClaudeProviderDefaults};
use super::runtime::model_name;
use super::setup::{ClaudeSetup, ClaudeStatus, SetupState, Step};
use crate::agent_plugins::process::Spawner;
use crate::agent_plugins::provider::{Choice, described_choice};
use crate::agent_plugins::tabs::{Picking, field, note_with_code};
use crate::agents2::{
    AgentProvider, CloseProvider, CreateModels, ModelChoice, ProviderSnapshot, Readiness,
    providers_changed, registry::AttentionAction,
};
use crate::runtime::Runtime;
use buzz_ui::{
    ActiveTheme, Button, Colors, Spinner, Surface, TextField, TypeStyled, tokens::space,
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::time::Duration;

/// How long listing Claude's models may take.
const MODELS_TIMEOUT: Duration = Duration::from_secs(15);

/// The line that says where Claude stands.
pub fn status_line(state: &SetupState) -> String {
    match (&state.status, state.checking) {
        (None, _) | (_, true) => "Checking Claude…".into(),
        (Some(ClaudeStatus::Missing), _) => "Claude is not installed.".into(),
        (Some(ClaudeStatus::Outdated { version }), _) => {
            format!("Claude {version} is older than Buzz needs.")
        }
        (Some(ClaudeStatus::SignedOut { version }), _) => {
            format!("Claude {version} needs you to sign in.")
        }
        (Some(ClaudeStatus::Ready { version, account }), _) => match account {
            Some(account) => format!("Claude {version} is ready ({account})."),
            None => format!("Claude {version} is ready."),
        },
        (Some(ClaudeStatus::Error { message }), _) => message.clone(),
    }
}

/// The menu's view of Claude. It follows the last status only, so a check
/// again keeps the last answer until the new one.
pub fn health(state: &SetupState) -> ProviderSnapshot {
    let (readiness, label) = match &state.status {
        None => (Readiness::Checking, None),
        Some(ClaudeStatus::Ready { .. }) => (Readiness::Ready, None),
        Some(ClaudeStatus::Missing) => (Readiness::Attention, Some("Install Claude Code")),
        Some(ClaudeStatus::Outdated { .. }) => (Readiness::Attention, Some("Update Claude Code")),
        Some(ClaudeStatus::SignedOut { .. }) => {
            (Readiness::Attention, Some("Log in to Claude Code"))
        }
        Some(ClaudeStatus::Error { .. }) => {
            (Readiness::Attention, Some("Claude Code needs attention"))
        }
    };
    ProviderSnapshot {
        readiness,
        attention_label: label.map(str::to_owned),
        blocking: state.running.is_some(),
    }
}

fn option_copy(model: &ClaudeModel) -> (String, Option<String>) {
    let label = match (&model.resolved_model, &model.display_name) {
        (Some(resolved), _) => model_name(resolved),
        (None, Some(name)) => name.clone(),
        (None, None) => model.value.clone(),
    };
    let prefix = format!("{label} · ");
    let description = model.description.as_ref().map(|description| {
        description
            .strip_prefix(&prefix)
            .unwrap_or(description)
            .to_owned()
    });
    (label, description)
}

/// The models Claude lists; `default` is the picker's own default row.
pub fn model_choices(models: &[ClaudeModel]) -> Vec<ModelChoice> {
    models
        .iter()
        .filter(|model| model.value != "default")
        .map(|model| {
            let (label, description) = option_copy(model);
            ModelChoice {
                id: model.value.clone(),
                label,
                description,
            }
        })
        .collect()
}

fn listed_name(model: &str, models: &[ClaudeModel]) -> String {
    model_choices(models)
        .into_iter()
        .find(|choice| choice.id == model)
        .map(|choice| choice.label)
        .unwrap_or_else(|| model_name(model))
}

/// "Claude default", with the model Claude recommends once it says.
pub fn native_label(models: &[ClaudeModel]) -> String {
    match models
        .iter()
        .find(|model| model.value == "default")
        .and_then(|model| model.resolved_model.as_deref())
    {
        Some(resolved) => format!("Claude default ({})", model_name(resolved)),
        None => "Claude default".into(),
    }
}

/// The model menu's default row: the device default when one is saved.
pub fn provider_default_label(model: &str, models: &[ClaudeModel]) -> String {
    if model.is_empty() {
        native_label(models)
    } else {
        format!("Provider default · {}", listed_name(model, models))
    }
}

/// Lists Claude's models with a process of its own, without a turn. Must
/// run inside tokio.
async fn query_models(spawn: Spawner) -> Result<Vec<ClaudeModel>, String> {
    let launch = Launch {
        cwd: "~/.buzz".into(),
        ..Default::default()
    };
    let process = ClaudeProcess::spawn(&spawn, &launch).await?;
    let listed = tokio::time::timeout(MODELS_TIMEOUT, async {
        process.initialize(&launch).await?;
        Ok::<_, String>(process.models().await)
    })
    .await
    .unwrap_or_else(|_| Err("Claude Code model discovery timed out".into()));
    process.kill().await;
    match listed? {
        models if models.is_empty() => Err("Claude did not list its models".into()),
        models => Ok(models),
    }
}

/// Emitted when the device-wide defaults change, which running agents
/// inherit.
pub struct DefaultsChanged;

/// Claude on the embedded host, its models and the device defaults.
pub struct ClaudeProvider {
    pub setup: ClaudeSetup,
    pub state: SetupState,
    spawn: Option<Spawner>,
    pub models: Vec<ClaudeModel>,
    pub loading_models: bool,
    pub models_error: Option<String>,
    pub defaults: ClaudeDefaults,
    _follow: Task<()>,
    _load: Option<Task<()>>,
}

impl EventEmitter<DefaultsChanged> for ClaudeProvider {}

impl ClaudeProvider {
    pub fn new(setup: ClaudeSetup, spawn: Option<Spawner>, cx: &mut Context<Self>) -> Self {
        let mut changes = setup.subscribe();
        let follow = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                let state = changes.borrow_and_update().clone();
                let alive = this.update(cx, |this, cx| {
                    let ready = matches!(state.status, Some(ClaudeStatus::Ready { .. }));
                    if !ready && state.status.is_some() {
                        this.models.clear();
                        this.models_error = None;
                    }
                    this.state = state;
                    cx.notify();
                    providers_changed(cx);
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        Self {
            state: setup.snapshot(),
            setup,
            spawn,
            models: Vec::new(),
            loading_models: false,
            models_error: None,
            defaults: ClaudeDefaults::load(),
            _follow: follow,
            _load: None,
        }
    }

    /// Checks Claude again, unless a check or a step is running.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        if self.state.checking || self.state.running.is_some() {
            return;
        }
        let setup = self.setup.clone();
        Runtime::handle(cx).spawn(async move { setup.check().await });
    }

    /// What a surface showing the catalog needs: a first check, then the
    /// models once Claude is ready. A failed load waits for Try again.
    pub fn ensure_catalog(&mut self, cx: &mut Context<Self>) {
        if self.state.status.is_none() && !self.state.checking {
            self.check(cx);
        }
        if matches!(self.state.status, Some(ClaudeStatus::Ready { .. }))
            && self.models.is_empty()
            && !self.loading_models
            && self.models_error.is_none()
        {
            self.load_models(cx);
        }
    }

    /// Loads the models Claude lists, for the surfaces that show them.
    pub fn load_models(&mut self, cx: &mut Context<Self>) {
        let Some(spawn) = self.spawn.clone() else {
            return;
        };
        if self.loading_models || !matches!(self.state.status, Some(ClaudeStatus::Ready { .. })) {
            return;
        }
        self.loading_models = true;
        self.models_error = None;
        let listed = Runtime::handle(cx).spawn(query_models(spawn));
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = listed
                .await
                .unwrap_or_else(|_| Err("Claude stopped".into()));
            let _ = this.update(cx, |this, cx| {
                this.loading_models = false;
                // Setup stays usable; the pickers say so and offer to try
                // again.
                match result {
                    Ok(models) => this.models = models,
                    Err(error) => this.models_error = Some(error),
                }
                cx.notify();
                providers_changed(cx);
            });
        }));
        cx.notify();
        providers_changed(cx);
    }

    pub fn save_defaults(
        &mut self,
        next: ClaudeProviderDefaults,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.defaults.save(next)?;
        cx.emit(DefaultsChanged);
        cx.notify();
        providers_changed(cx);
        Ok(())
    }

    /// Claude's rows in the New agent dialog's model menu.
    pub fn create_models(&self) -> CreateModels {
        CreateModels {
            models: model_choices(&self.models),
            default_label: provider_default_label(&self.defaults.snapshot().model, &self.models),
            default_description: Some("Following the provider's model default".into()),
            allow_custom: true,
            loading: false,
        }
    }
}

/// The type's provider, for the Agents page.
pub struct Provider(pub Entity<ClaudeProvider>);

impl AgentProvider for Provider {
    fn snapshot(&self, cx: &App) -> ProviderSnapshot {
        health(&self.0.read(cx).state)
    }

    fn check(&self, cx: &mut App) {
        self.0.update(cx, |provider, cx| provider.check(cx));
    }

    fn attention_action(&self, cx: &App) -> Option<AttentionAction> {
        let state = &self.0.read(cx).state;
        if state.running.is_some() {
            return None;
        }
        let step = match state.status {
            Some(ClaudeStatus::Missing | ClaudeStatus::Outdated { .. }) => Step::Install,
            Some(ClaudeStatus::SignedOut { .. }) => Step::Login,
            _ => return None,
        };
        let provider = self.0.clone();
        Some(Box::new(move |cx: &mut App| {
            let setup = provider.read(cx).setup.clone();
            Runtime::handle(cx).spawn(async move { setup.run(step).await });
        }))
    }

    fn dialog_body(&self, close: CloseProvider, window: &mut Window, cx: &mut App) -> AnyView {
        let provider = self.0.clone();
        cx.new(|cx| ProviderBody::new(provider, close, window, cx))
            .into()
    }
}

/// What Install does.
fn installer_note(c: &Colors) -> Div {
    note_with_code(
        "Downloads Buzz's own copy of Claude (about 250 MB) from downloads.claude.ai and keeps it on the latest release. It does not change the",
        "claude",
        "in your terminal, and uses the same sign-in and settings.",
        c,
    )
}

/// Claude's install and sign-in on a host: where it stands, the step to
/// take, the step's progress and output, and the sign-in code it may ask
/// for.
pub struct SetupControls {
    setup: ClaudeSetup,
    state: SetupState,
    code: Entity<InputState>,
    /// In the Claude tab, which also says when Claude is ready and offers
    /// Check again.
    in_tab: bool,
    _follow: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl SetupControls {
    pub fn new(
        setup: ClaudeSetup,
        in_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let code = cx.new(|cx| InputState::new(window, cx));
        let mut changes = setup.subscribe();
        let follow = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                let state = changes.borrow_and_update().clone();
                let alive = this.update(cx, |this, cx| {
                    this.state = state;
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        let subscriptions = vec![cx.subscribe(&code, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];
        Self {
            state: setup.snapshot(),
            setup,
            code,
            in_tab,
            _follow: follow,
            _subscriptions: subscriptions,
        }
    }

    fn run(&self, step: Step, cx: &mut Context<Self>) {
        let setup = self.setup.clone();
        Runtime::handle(cx).spawn(async move { setup.run(step).await });
    }

    fn send_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let code = self.code.read(cx).value().to_string();
        if code.trim().is_empty() {
            return;
        }
        let setup = self.setup.clone();
        Runtime::handle(cx).spawn(async move { setup.answer(&code).await });
        self.code
            .update(cx, |state, cx| state.set_value("", window, cx));
    }
}

impl Render for SetupControls {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let state = &self.state;
        let status = state.status.clone();
        let running = state.running;
        let action = match status {
            Some(ClaudeStatus::Missing) => Some((Step::Install, "Install Claude")),
            Some(ClaudeStatus::Outdated { .. }) => Some((Step::Install, "Update Claude")),
            Some(ClaudeStatus::SignedOut { .. }) => Some((Step::Login, "Sign in")),
            _ => None,
        };
        let line = div().text_color(c.text_standard).child(status_line(state));
        let line = if self.in_tab {
            line.text_body()
        } else {
            line.text_body_sm().font_weight(FontWeight::BOLD)
        };
        let progress = running.map(|step| {
            let latest = state
                .output
                .replace('\r', "\n")
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .map(str::to_owned);
            let text = match step {
                Step::Login => "Waiting for Claude Code sign-in…".to_string(),
                Step::Install => latest.unwrap_or_else(|| "Installing Claude Code…".into()),
            };
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(space::S2)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::S2)
                        .text_body_sm()
                        .child(Spinner::new().size(16.))
                        .child(text),
                )
                .child(
                    Button::new("claude-code-cancel", "Cancel").on_click(cx.listener(
                        |this, _, _, cx| {
                            let setup = this.setup.clone();
                            Runtime::handle(cx).spawn(async move { setup.cancel().await });
                        },
                    )),
                )
        });
        let buttons = (running.is_none() && (action.is_some() || self.in_tab)).then(|| {
            div()
                .flex()
                .flex_wrap()
                .gap(space::S2)
                .children(action.map(|(step, label)| {
                    Button::new("claude-code-step", label)
                        .prominent()
                        .on_click(cx.listener(move |this, _, _, cx| this.run(step, cx)))
                }))
                .when(self.in_tab, |this| {
                    this.child(
                        Button::new("claude-code-check", "Check again")
                            .ghost()
                            .disabled(state.checking)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let setup = this.setup.clone();
                                Runtime::handle(cx).spawn(async move { setup.check().await });
                            })),
                    )
                })
        });
        let has_code = !self.code.read(cx).value().trim().is_empty();
        div()
            .flex()
            .flex_col()
            .gap(space::S2)
            .children(
                state
                    .notice
                    .clone()
                    .map(|notice| div().text_body_sm().child(notice)),
            )
            .when(running.is_none(), |this| this.child(line))
            .children(progress)
            .children(buttons)
            .when(
                matches!(status, Some(ClaudeStatus::Missing)) && running.is_none(),
                |this| this.child(installer_note(&c)),
            )
            .when(
                !state.output.is_empty() && running != Some(Step::Install),
                |this| {
                    this.child(
                        div()
                            .id("claude-code-setup-output")
                            .max_h(rems(15.))
                            .overflow_y_scroll()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .whitespace_normal()
                            .child(state.output.clone()),
                    )
                },
            )
            .when(running == Some(Step::Login), |this| {
                this.child(
                    div()
                        .flex()
                        .items_end()
                        .gap(space::S2)
                        .child(
                            field(
                                "Sign-in code",
                                TextField::new(&self.code).surface(Surface::Floating),
                                Some("If the browser shows a code to paste, enter it here.".into()),
                                &c,
                            )
                            .flex_1(),
                        )
                        .child(
                            Button::new("claude-code-send-code", "Send")
                                .disabled(!has_code)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.send_code(window, cx)),
                                ),
                        ),
                )
            })
            .when(matches!(status, Some(ClaudeStatus::Error { .. })), |this| {
                this.child(div().text_body_sm().text_color(c.text_subtle).child(
                    "Open Claude Code in a terminal for more details, then please try again.",
                ))
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultModel;

/// The provider dialog's body: setup until Claude is ready, then the
/// device-wide default model.
pub struct ProviderBody {
    provider: Entity<ClaudeProvider>,
    controls: Entity<SetupControls>,
    draft: String,
    open: Option<DefaultModel>,
    error: Option<String>,
    close: CloseProvider,
    _subscriptions: Vec<Subscription>,
}

impl ProviderBody {
    fn new(
        provider: Entity<ClaudeProvider>,
        close: CloseProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let setup = provider.read(cx).setup.clone();
        let controls = cx.new(|cx| SetupControls::new(setup, false, window, cx));
        let draft = provider.read(cx).defaults.snapshot().model.clone();
        let subscriptions = vec![cx.observe(&provider, |_, provider, cx| {
            provider.update(cx, |provider, cx| provider.ensure_catalog(cx));
            cx.notify();
        })];
        provider.update(cx, |provider, cx| provider.ensure_catalog(cx));
        Self {
            provider,
            controls,
            draft,
            open: None,
            error: None,
            close,
            _subscriptions: subscriptions,
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = ClaudeProviderDefaults {
            model: self.draft.clone(),
        };
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
    type Picker = DefaultModel;

    fn open(&mut self) -> &mut Option<DefaultModel> {
        &mut self.open
    }

    fn pick(&mut self, _: DefaultModel, value: String, _: &mut Context<Self>) {
        self.draft = value;
    }
}

impl Render for ProviderBody {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let provider = self.provider.read(cx);
        let ready = matches!(provider.state.status, Some(ClaudeStatus::Ready { .. }))
            && !provider.state.checking;
        let saved = provider.defaults.snapshot().model.clone();
        let native = provider.models.iter().any(|model| model.value == "default");
        let mut choices = vec![
            Choice::new("", native_label(&provider.models))
                .described(native.then(|| "Following Claude Code's recommendation".to_string())),
        ];
        let listed = model_choices(&provider.models);
        // A saved default Claude has not listed (yet) stays selectable.
        let unlisted =
            !self.draft.is_empty() && !listed.iter().any(|choice| choice.id == self.draft);
        choices.extend(
            listed
                .into_iter()
                .map(|choice| Choice::new(choice.id, choice.label).described(choice.description)),
        );
        if unlisted {
            choices.push(Choice::new(self.draft.clone(), model_name(&self.draft)));
        }
        let loading = provider.loading_models;
        let models_error = provider.models_error.clone();
        let select = described_choice(
            self.open,
            DefaultModel,
            "claude-code-default-model",
            &self.draft.clone(),
            choices,
            false,
            window,
            cx,
        );
        let form = div()
            .flex()
            .flex_col()
            .gap(space::S3)
            .child(field(
                "Default model",
                select,
                loading.then(|| "Loading Claude's models…".into()),
                &c,
            ))
            .children(models_error.map(|error| {
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(space::S2)
                    .child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_danger)
                            .child(format!("Couldn't load Claude's models: {error}")),
                    )
                    .child(
                        Button::new("claude-code-models-retry", "Try again")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.provider
                                    .update(cx, |provider, cx| provider.load_models(cx))
                            })),
                    )
            }))
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            )
            .child(
                div().flex().child(
                    Button::new("claude-code-save-defaults", "Save defaults")
                        .prominent()
                        .disabled(self.draft == saved)
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                ),
            );
        div()
            .flex()
            .flex_col()
            .gap(space::S6)
            .text_body()
            .when(!ready, |this| this.child(self.controls.clone()))
            .when(ready, |this| this.child(form))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn model(value: &str, resolved: Option<&str>, description: Option<&str>) -> ClaudeModel {
        ClaudeModel {
            value: value.into(),
            resolved_model: resolved.map(Into::into),
            description: description.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn claude_readiness_follows_its_status() {
        let state = |status: Option<ClaudeStatus>| SetupState {
            status,
            ..Default::default()
        };
        assert_eq!(health(&state(None)), ProviderSnapshot::CHECKING);
        let missing = health(&state(Some(ClaudeStatus::Missing)));
        assert_eq!(
            (missing.readiness, missing.attention_label.as_deref()),
            (Readiness::Attention, Some("Install Claude Code"))
        );
        let signed_out = health(&state(Some(ClaudeStatus::SignedOut {
            version: "2.1.0".into(),
        })));
        assert_eq!(
            signed_out.attention_label.as_deref(),
            Some("Log in to Claude Code")
        );
        let broken = health(&state(Some(ClaudeStatus::Error {
            message: "boom".into(),
        })));
        assert_eq!(
            broken.attention_label.as_deref(),
            Some("Claude Code needs attention")
        );
        // Checking again keeps the last answer.
        let refreshing = SetupState {
            status: Some(ClaudeStatus::Ready {
                version: "2.1.0".into(),
                account: None,
            }),
            checking: true,
            ..Default::default()
        };
        assert_eq!(health(&refreshing).readiness, Readiness::Ready);
        let installing = SetupState {
            running: Some(Step::Install),
            ..state(Some(ClaudeStatus::Missing))
        };
        assert!(health(&installing).blocking);
    }

    #[test]
    fn labels_name_the_device_default_or_claudes_own() {
        let models = vec![
            model("default", Some("claude-opus-5-5"), None),
            model(
                "sonnet",
                Some("claude-sonnet-5"),
                Some("Sonnet 5 · Fast and capable"),
            ),
        ];
        assert_eq!(provider_default_label("", &[]), "Claude default");
        assert_eq!(
            provider_default_label("", &models),
            "Claude default (Opus 5.5)"
        );
        assert_eq!(
            provider_default_label("sonnet", &models),
            "Provider default · Sonnet 5"
        );
        assert_eq!(
            provider_default_label("claude-haiku-4-5", &models),
            "Provider default · Haiku 4.5"
        );
        let choices = model_choices(&models);
        assert_eq!(choices.len(), 1, "the default row is the picker's own");
        assert_eq!(
            choices[0].description.as_deref(),
            Some("Fast and capable"),
            "the name is not repeated"
        );
    }
}
