//! A development-only agent type, "Echo", for trying Agents2 without a
//! real type: it answers every mention with the mention's text. Debug
//! builds register it when `BUZZ_DEV_AGENTS2` is set; release builds never
//! include it.

use super::Agent;
use super::attention::{AttentionValue, Interest};
use super::registry::{AgentTab, AgentType, AgentView, TypeDefaults, register_type};
use super::runtime::{Delivery, Trigger};
use buzz_host::api::AppAgentEvent;
use buzz_ui::{ActiveTheme, Button, ButtonSize, Surface, TextField, TypeStyled};
use gpui_kit::component::input::InputState;
use gpui_kit::*;
use serde_json::json;
use std::sync::Arc;

fn prefix(agent: &Agent) -> String {
    agent
        .config
        .get("prefix")
        .and_then(|prefix| prefix.as_str())
        .unwrap_or("echo: ")
        .to_owned()
}

/// The Echo tab: the reply prefix.
struct EchoTab {
    view: AgentView,
    prefix: Entity<InputState>,
    saved: bool,
}

impl EchoTab {
    fn new(view: AgentView, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current = prefix(&view.agent);
        let prefix = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value(current, window, cx);
            state
        });
        Self {
            view,
            prefix,
            saved: false,
        }
    }
}

impl Render for EchoTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = cx.colors();
        div()
            .flex()
            .flex_col()
            .gap(rems(0.75))
            .child(div().text_label_sm().child("Reply prefix"))
            .child(TextField::new(&self.prefix).surface(Surface::Floating))
            .child(
                div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(if self.saved {
                        "Saved."
                    } else {
                        "Put before every echo."
                    }),
            )
            .child(
                div().flex().child(
                    Button::new("echo-save", "Save")
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let mut config = this.view.agent.config.clone();
                            config["prefix"] = json!(this.prefix.read(cx).value().to_string());
                            let task = this.view.save(config, cx);
                            cx.spawn(async move |this, cx| {
                                let saved = task.await.is_ok();
                                let _ = this.update(cx, |this, cx| {
                                    this.saved = saved;
                                    cx.notify();
                                });
                            })
                            .detach();
                        })),
                ),
            )
    }
}

struct Echo;

impl AgentType for Echo {
    fn id(&self) -> &'static str {
        "echo"
    }

    fn title(&self) -> SharedString {
        "Echo".into()
    }

    fn description(&self) -> Option<SharedString> {
        Some("Development only: answers every mention with its text.".into())
    }

    fn defaults(&self) -> TypeDefaults {
        TypeDefaults {
            config: json!({ "prefix": "echo: " }),
            attention: [(
                "interest/default".to_owned(),
                AttentionValue::Interest(Interest {
                    instructions: "Answer every mention.".into(),
                }),
            )]
            .into(),
        }
    }

    fn summary(&self, agent: &Agent) -> Option<String> {
        Some(format!(
            "Replies \u{201c}{}\u{2026}\u{201d}",
            prefix(agent).trim()
        ))
    }

    fn peek(&self, agent: &Agent, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let c = cx.colors();
        Some(
            div()
                .flex()
                .flex_col()
                .gap(rems(0.25))
                .text_body_sm()
                .child(div().text_label_sm().child("Echo"))
                .child(div().text_color(c.text_subtle).child(format!(
                    "Answers mentions with \u{201c}{}\u{2026}\u{201d}",
                    prefix(agent).trim()
                )))
                .into_any_element(),
        )
    }

    fn tabs(&self) -> Vec<AgentTab> {
        vec![AgentTab {
            id: "echo".into(),
            title: "Echo".into(),
            build: Arc::new(|view, window, cx| cx.new(|cx| EchoTab::new(view, window, cx)).into()),
        }]
    }

    fn runs(&self) -> bool {
        true
    }

    fn run(&self, delivery: Delivery, cx: &mut App) -> Task<Result<(), String>> {
        let Trigger::Mention { event } = delivery.trigger else {
            return Task::ready(Ok(()));
        };
        let Some(channel) = event.tag("h").map(str::to_owned).or(delivery.channel) else {
            return Task::ready(Ok(()));
        };
        let prefix = delivery
            .config
            .get("prefix")
            .and_then(|prefix| prefix.as_str())
            .unwrap_or("echo: ")
            .to_owned();
        let agent = delivery.agent;
        cx.background_spawn(async move {
            agent
                .publish(AppAgentEvent {
                    kind: 9,
                    content: format!("{prefix}{}", event.content),
                    tags: vec![
                        vec!["h".into(), channel],
                        vec!["e".into(), event.id.clone(), String::new(), "reply".into()],
                        vec!["p".into(), event.pubkey.clone()],
                    ],
                })
                .await
                .map(drop)
        })
    }
}

pub(super) fn init(cx: &mut App) {
    if std::env::var_os("BUZZ_DEV_AGENTS2").is_some() {
        register_type("buzz.dev", Arc::new(Echo), cx).expect("the echo type is valid");
    }
}
