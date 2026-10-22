//! What the agent launches with: the harness, its model and instructions,
//! and the provider config and environment, edited as JSON.

use super::config::{Config, RUNTIMES};
use crate::agent_plugins::tabs::{
    Picking, choice, field, instructions_field, instructions_state, saved_config,
};
use crate::agents2::{AgentView, Agents2};
use buzz_ui::{ActiveTheme, Button, Surface, TextArea, TextField, TypeStyled, tokens::space};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
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
    provider_config: Entity<TextareaState>,
    env: Entity<TextareaState>,
    instructions: Entity<TextareaState>,
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

/// The draft as a config: the JSON fields parsed, and the whole checked.
pub fn parse(
    saved: &Config,
    runtime: &str,
    model: &str,
    provider_config: &str,
    env: &str,
    instructions: &str,
) -> Result<Config, String> {
    let provider_config: Value = serde_json::from_str(provider_config)
        .map_err(|error| format!("The provider config is not valid JSON: {error}"))?;
    if !provider_config.is_object() {
        return Err("The provider config must be a JSON object.".into());
    }
    let env: Value = serde_json::from_str(env)
        .map_err(|error| format!("The environment is not valid JSON: {error}"))?;
    let all_text = env
        .as_object()
        .is_some_and(|env| env.values().all(Value::is_string));
    if !all_text {
        return Err("The environment must be a JSON object of text values.".into());
    }
    let mut draft = saved.value();
    draft["runtime"] = runtime.into();
    draft["model"] = model.trim().into();
    draft["provider_config"] = provider_config;
    draft["env"] = env;
    draft["instructions"] = instructions.into();
    let config = Config::read(&draft);
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
        let provider_config = textarea(&pretty(&saved.provider_config), window, cx);
        let env = textarea(&pretty(&saved.env), window, cx);
        let instructions = instructions_state(&saved.instructions, window, cx);
        let mut subscriptions = vec![cx.subscribe(&model, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];
        for area in [&provider_config, &env, &instructions] {
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
            provider_config,
            env,
            instructions,
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
        for (area, text) in [
            (&self.provider_config, pretty(&saved.provider_config)),
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
            &self.runtime,
            &self.model.read(cx).value(),
            &self.provider_config.read(cx).value(),
            &self.env.read(cx).value(),
            &self.instructions.read(cx).value(),
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

impl Picking for SettingsTab {
    type Picker = Picker;

    fn open(&mut self) -> &mut Option<Picker> {
        &mut self.open
    }

    fn pick(&mut self, _: Picker, value: String, _: &mut Context<Self>) {
        self.runtime = value;
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
                "Provider config",
                TextArea::new(&self.provider_config)
                    .rows(10)
                    .surface(Surface::Floating)
                    .disabled(saving),
                Some(
                    "buzz-backend-kubernetes settings, such as context, namespace and image. Always a Sandbox per thread; no secrets."
                        .into(),
                ),
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
