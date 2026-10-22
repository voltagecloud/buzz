//! What the agent launches with: the Sandbox manager's relay and key, the
//! harness, its model and instructions, and the environment, beside the
//! form that enrolls a developer profile with the manager.

use super::config::{Config, RUNTIMES};
use super::enroll_form::{self, EnrollForm};
use crate::agent_plugins::tabs::{
    Picking, choice, field, instructions_field, instructions_state, saved_config,
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
}

pub struct SettingsTab {
    view: AgentView,
    /// The saved settings the draft started from.
    saved: Config,
    runtime: String,
    model: Entity<InputState>,
    relay_url: Entity<InputState>,
    manager_pubkey: Entity<InputState>,
    env: Entity<TextareaState>,
    instructions: Entity<TextareaState>,
    enroll: EnrollForm,
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
    pub relay_url: &'a str,
    pub manager_pubkey: &'a str,
    pub env: &'a str,
    pub instructions: &'a str,
}

/// The saved relay and manager key, for their fields. The relay is the
/// agent's community, `community`, until one is saved.
pub fn relay_fields(saved: &Config, community: &str) -> (String, String) {
    let text = |name: &str| {
        saved
            .provider_config
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    (
        text("relay_url").unwrap_or_else(|| community.to_owned()),
        text("manager_pubkey").unwrap_or_default(),
    )
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
        let (relay, manager) = relay_fields(&saved, &view.agent.relay);
        let relay_url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("wss://")
                .default_value(relay)
        });
        let manager_pubkey = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("64 hex characters")
                .default_value(manager)
        });
        let env = textarea(&pretty(&saved.env), window, cx);
        let instructions = instructions_state(&saved.instructions, window, cx);
        let enroll = EnrollForm::new("", window, cx);
        let mut subscriptions = Vec::new();
        for input in [&model, &relay_url, &manager_pubkey]
            .into_iter()
            .chain(enroll.inputs())
        {
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
        Self {
            view,
            runtime: saved.runtime.clone(),
            saved,
            model,
            relay_url,
            manager_pubkey,
            env,
            instructions,
            enroll,
            open: None,
            saving: false,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn reset(&mut self, saved: Config, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |state, cx| {
            state.set_value(saved.model.clone(), window, cx)
        });
        let (relay, manager) = relay_fields(&saved, &self.view.agent.relay);
        for (input, text) in [(&self.relay_url, relay), (&self.manager_pubkey, manager)] {
            input.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        for (area, text) in [
            (&self.env, pretty(&saved.env)),
            (&self.instructions, saved.instructions.clone()),
        ] {
            area.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        self.runtime = saved.runtime.clone();
        self.saved = saved;
        cx.notify();
    }

    fn draft(&self, cx: &App) -> Result<Config, String> {
        parse(
            &self.saved,
            Draft {
                runtime: &self.runtime,
                model: &self.model.read(cx).value(),
                relay_url: &self.relay_url.read(cx).value(),
                manager_pubkey: &self.manager_pubkey.read(cx).value(),
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
    /// Sends the enroll form to the manager in the relay fields, as the
    /// signed-in owner. Its tokens are cleared at once, sent or not.
    fn enroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let relay = self.relay_url.read(cx).value().trim().to_owned();
        let manager = self.manager_pubkey.read(cx).value().trim().to_owned();
        let owner = enroll_form::owner(cx);
        let pubkey = owner
            .as_ref()
            .map(|owner| owner.pubkey_hex())
            .unwrap_or_default();
        let profile = self.enroll.submit(&pubkey, window, cx);
        let relay_config = Config {
            provider_config: Config::relay(&relay, &manager),
            ..Config::default()
        };
        let started = (|| {
            let owner = owner.ok_or("Sign in to enroll a profile.")?;
            relay_config.validate()?;
            let session = enroll_form::session(&relay, cx)
                .ok_or("Join the community at the relay to enroll through it.")?;
            Ok::<_, String>((owner, session, profile?))
        })();
        let (owner, session, profile) = match started {
            Ok(started) => started,
            Err(error) => {
                self.enroll.status = Some(Err(error));
                cx.notify();
                return;
            }
        };
        self.enroll.sending = true;
        self.enroll.status = None;
        let sent = crate::runtime::Runtime::handle(cx)
            .spawn(super::enroll::enroll(session, owner, manager, profile));
        cx.spawn(async move |this, cx| {
            let outcome = sent
                .await
                .unwrap_or_else(|_| Err("The enrollment was cancelled.".into()));
            let _ = this.update(cx, |this, cx| {
                this.enroll.sending = false;
                this.enroll.status = Some(outcome);
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
                        None,
                        &c,
                    ))),
            )
            .child(field(
                    "Relay",
                    TextField::new(&self.relay_url)
                        .surface(Surface::Floating)
                        .disabled(saving),
                    Some("Where the Sandbox manager listens; the agent's community by default.".into()),
                    &c,
                ))
                .child(field(
                    "Manager key",
                    TextField::new(&self.manager_pubkey)
                        .surface(Surface::Floating)
                        .disabled(saving),
                    Some("The Sandbox manager's public key, in hex.".into()),
                    &c,
                ))
            .child(field(
                "Environment",
                TextArea::new(&self.env)
                    .rows(6)
                    .surface(Surface::Floating)
                    .disabled(saving),
                Some(
                    "A JSON object, such as BUZZ_CHECKPOINT_BUCKET and BUZZ_CHECKPOINT_PREFIX. Saved on this device as plain text."
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
                this.child(self.enroll.render(owner.as_deref(), sending, &c))
                    .child(
                        div().flex().child(
                            Button::new(
                                "remote-workspace-enroll",
                                if sending { "Enrolling…" } else { "Enroll profile" },
                            )
                            .loading(sending)
                            .disabled(sending)
                            .on_click(cx.listener(|this, _, window, cx| this.enroll(window, cx))),
                        ),
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
