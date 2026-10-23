//! What the agent launches with: the harness, its model, effort and fast
//! mode, its instructions, and the environment, beside the form that
//! enrolls a developer profile with the Sandbox manager. Every agent asks the
//! manager over its community's relay, with the build's manager key.

use super::config::{Config, EFFORTS, MANAGER_PUBKEY, RUNTIMES};
use super::enroll::{OnFile, Outcome};
use super::enroll_form::{self, EnrollForm};
use crate::agent_plugins::tabs::{
    Picking, choice, field, instructions_field, instructions_state, options, saved_config,
};
use crate::agents2::{AgentView, Agents2};
use buzz_ui::{ActiveTheme, Button, Surface, TextArea, TextField, TypeStyled, tokens::space};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::Value;

#[derive(Clone, Copy, PartialEq)]
pub enum Picker {
    Runtime,
    Effort,
    Fast,
}

/// The Fast mode choices.
const FAST: &[(&str, &str)] = &[("off", "Off"), ("on", "On")];

pub struct SettingsTab {
    view: AgentView,
    /// The saved settings the draft started from.
    saved: Config,
    runtime: String,
    model: Entity<InputState>,
    effort: String,
    fast: bool,
    env: Entity<TextareaState>,
    instructions: Entity<TextareaState>,
    pub(super) enroll: EnrollForm,
    open: Option<Picker>,
    saving: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// `value` as indented JSON, for editing.
fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn textarea(value: &str, window: &mut Window, cx: &mut App) -> Entity<TextareaState> {
    let state = cx.new(|cx| TextareaState::new(window, cx));
    state.update(cx, |state, cx| {
        state.set_value(value.to_owned(), window, cx)
    });
    state
}

/// The tab's fields, as text.
#[derive(Clone, Copy, Default)]
pub struct Draft<'a> {
    pub runtime: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub fast: bool,
    /// The agent's community, and the manager key: neither is edited here.
    pub relay_url: &'a str,
    pub manager_pubkey: &'a str,
    pub env: &'a str,
    pub instructions: &'a str,
}

/// The draft as a config: the fields parsed, and the whole checked.
pub fn parse(saved: &Config, draft: Draft) -> Result<Config, String> {
    let provider_config = Config::relay(draft.relay_url, draft.manager_pubkey);
    let env: Value = serde_json::from_str(draft.env)
        .map_err(|error| format!("The environment is not valid JSON: {error}"))?;
    let all_text = env
        .as_object()
        .is_some_and(|env| env.values().all(Value::is_string));
    if !all_text {
        return Err("The environment must be a JSON object of text values.".into());
    }
    let mut value = saved.value();
    value["runtime"] = draft.runtime.into();
    value["model"] = draft.model.trim().into();
    value["effort"] = draft.effort.into();
    value["fast"] = draft.fast.into();
    value["provider_config"] = provider_config;
    value["env"] = env;
    value["instructions"] = draft.instructions.into();
    let config = Config::read(&value);
    config.validate()?;
    Ok(config)
}

impl SettingsTab {
    pub fn new(view: AgentView, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let saved = Config::read(&saved_config(&view, cx));
        let model = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("The harness's default")
                .default_value(saved.model.clone())
        });
        let env = textarea(&pretty(&saved.env), window, cx);
        let instructions = instructions_state(&saved.instructions, window, cx);
        let enroll = EnrollForm::new(window, cx);
        let mut subscriptions = Vec::new();
        for input in std::iter::once(&model).chain(enroll.inputs()) {
            subscriptions.push(cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
        }
        for area in [&env, &instructions] {
            subscriptions.push(cx.subscribe(area, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
        }
        // Saved settings that change, here or elsewhere, replace the draft.
        if let Some(agents) = Agents2::global(cx) {
            subscriptions.push(cx.observe_in(&agents, window, |this, _, window, cx| {
                let saved = Config::read(&saved_config(&this.view, cx));
                if saved != this.saved {
                    this.reset(saved, window, cx);
                }
            }));
        }
        let mut tab = Self {
            view,
            runtime: saved.runtime.clone(),
            effort: saved.effort.clone(),
            fast: saved.fast,
            saved,
            model,
            env,
            instructions,
            enroll,
            open: None,
            saving: false,
            error: None,
            _subscriptions: subscriptions,
        };
        tab.load_on_file(window, cx);
        tab
    }

    fn reset(&mut self, saved: Config, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |state, cx| {
            state.set_value(saved.model.clone(), window, cx)
        });
        for (area, text) in [
            (&self.env, pretty(&saved.env)),
            (&self.instructions, saved.instructions.clone()),
        ] {
            area.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        self.runtime = saved.runtime.clone();
        self.effort = saved.effort.clone();
        self.fast = saved.fast;
        self.saved = saved;
        cx.notify();
    }

    fn draft(&self, cx: &App) -> Result<Config, String> {
        parse(
            &self.saved,
            Draft {
                runtime: &self.runtime,
                model: &self.model.read(cx).value(),
                effort: &self.effort,
                fast: self.fast,
                relay_url: &self.view.agent.relay,
                manager_pubkey: MANAGER_PUBKEY,
                env: &self.env.read(cx).value(),
                instructions: &self.instructions.read(cx).value(),
            },
        )
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let config = match self.draft(cx) {
            Ok(config) => config,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.error = None;
        self.saving = true;
        let save = self.view.save(config.value(), cx);
        cx.spawn(async move |this, cx| {
            let result = save.await;
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                this.error = result.err();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl SettingsTab {
    /// Asks the Sandbox manager what is on file for the signed-in owner.
    fn load_on_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let started = (|| {
            let owner = enroll_form::owner(cx).ok_or("Sign in first.")?;
            let session = enroll_form::session(&self.view.agent.relay, cx)
                .ok_or("Join the community at the relay first.")?;
            Ok::<_, String>((owner, session))
        })();
        let (owner, session) = match started {
            Ok(started) => started,
            Err(error) => {
                self.enroll.show_on_file(OnFile::Unknown(error), window, cx);
                cx.notify();
                return;
            }
        };
        self.enroll.show_on_file(OnFile::Loading, window, cx);
        let asked = crate::runtime::Runtime::handle(cx).spawn(super::enroll::on_file(
            session,
            owner,
            MANAGER_PUBKEY.to_owned(),
        ));
        cx.spawn_in(window, async move |this, cx| {
            let on_file = match asked.await {
                Ok(Ok(Some(summary))) => OnFile::Stored(summary),
                Ok(Ok(None)) => OnFile::Nothing,
                Ok(Err(error)) => OnFile::Unknown(error),
                Err(_) => OnFile::Unknown("The request was cancelled.".into()),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.enroll.show_on_file(on_file, window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Sends the enroll form to the Sandbox manager on the agent's
    /// community, as the signed-in owner. Its tokens are cleared at once,
    /// sent or not. With a profile on file, only what changes is sent.
    fn enroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let relay = self.view.agent.relay.clone();
        let owner = enroll_form::owner(cx);
        let values = self.enroll.submit(window, cx);
        let started = (|| {
            let owner = owner.ok_or("Sign in to enroll a profile.")?;
            let session = enroll_form::session(&relay, cx)
                .ok_or("Join the community at the relay to enroll through it.")?;
            Ok::<_, String>((owner, session, values))
        })();
        let (owner, session, values) = match started {
            Ok(started) => started,
            Err(error) => {
                self.enroll.status = Some(Err(error));
                cx.notify();
                return;
            }
        };
        self.enroll.sending = true;
        self.enroll.status = None;
        self.enroll.registration = None;
        let sent = crate::runtime::Runtime::handle(cx).spawn(super::enroll::run(
            session,
            owner,
            MANAGER_PUBKEY.to_owned(),
            values,
            self.enroll.on_file.summary().cloned(),
            self.enroll.replace_key,
            self.enroll.removed.clone(),
        ));
        cx.spawn_in(window, async move |this, cx| {
            let outcome = sent
                .await
                .unwrap_or_else(|_| Err("The enrollment was cancelled.".into()));
            let _ = this.update_in(cx, |this, window, cx| {
                this.enroll.sending = false;
                let (status, registration) = match outcome {
                    Ok((outcome, registration)) => (Ok(outcome), registration),
                    Err(error) => (Err(error), None),
                };
                if let Ok(Outcome::Enrolled { summary, .. }) = &status {
                    this.enroll.removed.clear();
                    this.enroll.replace_key = false;
                    match summary.clone() {
                        Some(summary) => {
                            this.enroll
                                .show_on_file(OnFile::Stored(summary), window, cx)
                        }
                        None => this.load_on_file(window, cx),
                    }
                }
                this.enroll.status = Some(status);
                this.enroll.registration = registration;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Picking for SettingsTab {
    type Picker = Picker;

    fn open(&mut self) -> &mut Option<Picker> {
        &mut self.open
    }

    fn pick(&mut self, picker: Picker, value: String, _: &mut Context<Self>) {
        match picker {
            Picker::Runtime => self.runtime = value,
            Picker::Effort => self.effort = value,
            Picker::Fast => self.fast = value == "on",
        }
    }
}

impl Render for SettingsTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        // A draft that does not parse counts as changed, so Save can say why.
        let changed = self.draft(cx).as_ref() != Ok(&self.saved);
        let saving = self.saving;
        let runtimes = RUNTIMES
            .iter()
            .map(|runtime| (runtime.id.to_string(), runtime.label.to_string()))
            .collect();
        let model_help = RUNTIMES
            .iter()
            .find(|runtime| runtime.id == self.runtime)
            .map(|runtime| runtime.model_help)
            .unwrap_or_default();
        div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .children(self.view.type_control(changed, window, cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(space::S4)
                    .child(div().flex_1().min_w(rems(14.)).child(field(
                        "Harness",
                        choice(
                            self.open,
                            Picker::Runtime,
                            "remote-workspace-runtime",
                            &self.runtime,
                            runtimes,
                            saving,
                            window,
                            cx,
                        ),
                        Some("Run from the image's PATH.".into()),
                        &c,
                    )))
                    .child(div().flex_1().min_w(rems(14.)).child(field(
                        "Model",
                        TextField::new(&self.model)
                            .surface(Surface::Floating)
                            .disabled(saving),
                        Some(model_help.into()),
                        &c,
                    ))),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(space::S4)
                    .child(div().flex_1().min_w(rems(14.)).child(field(
                        "Effort",
                        choice(
                            self.open,
                            Picker::Effort,
                            "remote-workspace-effort",
                            &self.effort,
                            options(EFFORTS),
                            saving,
                            window,
                            cx,
                        ),
                        Some("How hard the model thinks, on models that offer it.".into()),
                        &c,
                    )))
                    .child(div().flex_1().min_w(rems(14.)).child(field(
                        "Fast mode",
                        choice(
                            self.open,
                            Picker::Fast,
                            "remote-workspace-fast",
                            if self.fast { "on" } else { "off" },
                            options(FAST),
                            saving,
                            window,
                            cx,
                        ),
                        Some("Faster output on models that offer it, at higher usage.".into()),
                        &c,
                    ))),
            )
            .child(field(
                "Environment",
                TextArea::new(&self.env)
                    .rows(6)
                    .surface(Surface::Floating)
                    .disabled(saving),
                Some(
                    "Usually empty: extra environment for the Sandbox, as a JSON object. Saved on this device as plain text, so never put secrets here."
                        .into(),
                ),
                &c,
            ))
            .child(instructions_field(
                &self.instructions,
                saving,
                "Sent as the harness's system prompt when a thread's Sandbox starts.",
                &c,
            ))
            .map(|this| {
                let sending = self.enroll.sending;
                let owner = enroll_form::owner(cx).map(|owner| owner.pubkey_hex());
                let unknown = matches!(self.enroll.on_file, OnFile::Unknown(_));
                this.child(self.enroll.render(owner.as_deref(), sending, &c, cx))
                    .child(
                        div()
                            .flex()
                            .gap(space::S2)
                            .child(
                                Button::new(
                                    "remote-workspace-enroll",
                                    if sending { "Enrolling…" } else { "Enroll profile" },
                                )
                                .loading(sending)
                                .disabled(sending)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.enroll(window, cx)),
                                ),
                            )
                            .when(unknown, |row| {
                                row.child(
                                    Button::new("remote-workspace-check-on-file", "Check again")
                                        .outline()
                                        .disabled(sending)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.load_on_file(window, cx)
                                        })),
                                )
                            }),
                    )
            })
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            )
            .child(
                div().flex().child(
                    Button::new("remote-workspace-save", if saving { "Saving…" } else { "Save" })
                        .prominent()
                        .loading(saving)
                        .disabled(saving || !(changed || self.view.pending()))
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                ),
            )
    }
}
