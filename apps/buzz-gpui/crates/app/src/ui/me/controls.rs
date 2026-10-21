//! The chat controls under Me's composer (#1005): the follow-up assistant
//! (`SessionAgentControl`, `AgentChoice`) and the project
//! (`SessionProjectControl`).
//!
//! The selected assistant is device-local view intent per account,
//! community and chat (`selected-assistant.ts`), never Canvas, group data
//! or channel metadata. A plain send goes to it; an explicit mention
//! overrides it, and with none selected a message may go unaddressed.

use super::MeModel;
use super::recipients;
use crate::community::{AppState, Community};
use crate::ui::identity::AgentLibrary;
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_relay_client::sidebar_store::PreferencesStatus;
use buzz_ui::{ActiveTheme, Button, ButtonSize, IconName, Menu, MenuItem, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// The chat's selected assistant, or empty.
pub fn read_assistant(community: &Entity<Community>, channel: &str, cx: &App) -> String {
    cx.global::<AppState>()
        .store
        .view_state(&community.read(cx).partition(), &assistant_key(channel))
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<String>(&json).ok())
        .filter(|key| is_key(key))
        .unwrap_or_default()
}

/// Saves the chat's selected assistant; anything but a key clears it.
pub fn write_assistant(community: &Entity<Community>, channel: &str, agent: &str, cx: &App) {
    let value = if is_key(agent) { agent } else { "" };
    let json = serde_json::to_string(value).expect("strings serialize");
    if let Err(error) = cx.global::<AppState>().store.set_view_state(
        &community.read(cx).partition(),
        &assistant_key(channel),
        Some(&json),
    ) {
        tracing::warn!(%error, "could not save the selected assistant");
    }
}

fn assistant_key(channel: &str) -> String {
    format!("me:assistant:{channel}")
}

fn is_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The assistant changed.
pub struct AssistantChosen(pub String);

/// "Choose follow-up assistant": the viewer's agents in this community,
/// and "No assistant" to clear the choice.
pub struct AssistantControl {
    community: Entity<Community>,
    value: String,
    disabled: bool,
    open: bool,
}

impl EventEmitter<AssistantChosen> for AssistantControl {}

impl AssistantControl {
    pub fn new(community: Entity<Community>, value: String) -> Self {
        Self {
            community,
            value,
            disabled: false,
            open: false,
        }
    }

    pub fn set_value(&mut self, value: String, cx: &mut Context<Self>) {
        if self.value != value {
            self.value = value;
            cx.notify();
        }
    }

    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.disabled != disabled {
            self.disabled = disabled;
            if disabled {
                self.open = false;
            }
            cx.notify();
        }
    }

    fn choose(&mut self, agent: String, cx: &mut Context<Self>) {
        self.open = false;
        self.value = agent.clone();
        cx.emit(AssistantChosen(agent));
        cx.notify();
    }

    /// The selectable agents with their names, in host order.
    fn agents(&self, cx: &App) -> Vec<(String, String)> {
        let rules = recipients::rules(&self.community, cx);
        let library = AgentLibrary::from_hosts(cx);
        rules
            .selectable
            .into_iter()
            .map(|key| {
                let name = library
                    .agents
                    .iter()
                    .find(|(agent, _, _)| *agent == key)
                    .map(|(_, name, _)| name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| short(&key));
                (key, name)
            })
            .collect()
    }
}

fn short(key: &str) -> String {
    key.chars().take(10).collect()
}

impl Render for AssistantControl {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let agents = self.agents(cx);
        let selected = agents.iter().find(|(key, _)| *key == self.value);
        let label = match (selected, self.value.is_empty()) {
            (Some((_, name)), _) => name.clone(),
            (None, false) => short(&self.value),
            (None, true) => "Choose follow-up assistant".to_owned(),
        };
        let menu = self.open.then(|| {
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.open = false;
                    cx.notify();
                });
            });
            let mut content = Menu::new();
            if !self.value.is_empty() {
                content = content.child(
                    MenuItem::new("me-assistant-none", "No assistant")
                        .icon(IconName::Robot)
                        .icon_size(16.)
                        .on_click(cx.listener(|this, _, _, cx| this.choose(String::new(), cx))),
                );
            }
            for (index, (key, name)) in agents.iter().enumerate() {
                let key = key.clone();
                content = content.child(
                    MenuItem::new(("me-assistant", index), name.clone())
                        .highlighted(key == self.value)
                        .on_click(cx.listener(move |this, _, _, cx| this.choose(key.clone(), cx))),
                );
            }
            if agents.is_empty() {
                content = content.child(
                    MenuItem::new("me-assistant-empty", "No agents are available here.")
                        .disabled(true),
                );
            }
            overlay::dropup(window, Align::Start, content, dismiss)
        });
        div()
            .relative()
            .min_w_0()
            .child(
                Button::new("me-assistant", label)
                    .size(ButtonSize::Sm)
                    .ghost()
                    .icon(IconName::Robot)
                    .icon_end(IconName::CaretUp)
                    .disabled(self.disabled)
                    .selected(self.open)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open = !this.open;
                        cx.notify();
                    })),
            )
            .children(menu)
    }
}

/// The project changed (a new chat's local choice).
pub struct ProjectChosen(pub String);

/// Where the choice goes: a saved chat's Me assignment, or a new chat's
/// local selection.
enum Target {
    Chat(String),
    Draft(String),
}

/// "No project" or the chat's project, chosen from the Me groups.
pub struct ProjectControl {
    community: Entity<Community>,
    model: Entity<MeModel>,
    target: Target,
    disabled: bool,
    open: bool,
    busy: bool,
    error: Option<SharedString>,
    _model: Subscription,
}

impl EventEmitter<ProjectChosen> for ProjectControl {}

impl ProjectControl {
    pub fn chat(community: Entity<Community>, channel: String, cx: &mut Context<Self>) -> Self {
        Self::with(community, Target::Chat(channel), cx)
    }

    pub fn draft(community: Entity<Community>, project: String, cx: &mut Context<Self>) -> Self {
        Self::with(community, Target::Draft(project), cx)
    }

    fn with(community: Entity<Community>, target: Target, cx: &mut Context<Self>) -> Self {
        let model = MeModel::global(cx);
        Self {
            _model: cx.observe(&model, |_, _, cx| cx.notify()),
            community,
            model,
            target,
            disabled: false,
            open: false,
            busy: false,
            error: None,
        }
    }

    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.disabled != disabled {
            self.disabled = disabled;
            if disabled {
                self.open = false;
            }
            cx.notify();
        }
    }

    fn current(&self, cx: &App) -> String {
        match &self.target {
            Target::Draft(project) => project.clone(),
            Target::Chat(channel) => self
                .model
                .read(cx)
                .me
                .data
                .as_ref()
                .and_then(|data| data.assignments.get(channel).cloned())
                .unwrap_or_default(),
        }
    }

    fn choose(&mut self, project: String, cx: &mut Context<Self>) {
        self.open = false;
        self.error = None;
        match &mut self.target {
            Target::Draft(current) => {
                *current = project.clone();
                cx.emit(ProjectChosen(project));
            }
            Target::Chat(channel) => {
                let (channel, kit) = (
                    channel.clone(),
                    self.community.read(cx).session.me_kit().clone(),
                );
                self.busy = true;
                let section = (!project.is_empty()).then_some(project);
                cx.spawn(async move |this, cx| {
                    let result = kit.me_assign(&channel, section).await;
                    let _ = this.update(cx, |this, cx| {
                        this.busy = false;
                        this.error = result.err().map(Into::into);
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }
}

impl Render for ProjectControl {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let me = self.model.read(cx).me.clone();
        let current = self.current(cx);
        let projects: Vec<(String, String)> = me
            .data
            .as_ref()
            .map(|data| {
                data.sections
                    .iter()
                    .map(|section| (section.id.clone(), section.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let name = projects
            .iter()
            .find(|(id, _)| *id == current)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| "No project".into());
        let loading = matches!(
            me.status,
            PreferencesStatus::Idle | PreferencesStatus::Loading
        );
        let failed = matches!(me.status, PreferencesStatus::Error(_));
        let menu = self.open.then(|| {
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.open = false;
                    cx.notify();
                });
            });
            let mut content = Menu::new().child(
                MenuItem::new("me-project-none", "No project")
                    .highlighted(current.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.choose(String::new(), cx))),
            );
            for (index, (id, name)) in projects.iter().enumerate() {
                let id = id.clone();
                content = content.child(
                    MenuItem::new(("me-project", index), name.clone())
                        .highlighted(id == current)
                        .on_click(cx.listener(move |this, _, _, cx| this.choose(id.clone(), cx))),
                );
            }
            if failed {
                let kit = self.community.read(cx).session.me_kit().clone();
                content = content.child(
                    MenuItem::new("me-project-retry", "Retry projects").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.open = false;
                            kit.me_refresh();
                            cx.notify();
                        },
                    )),
                );
            }
            overlay::dropup(window, Align::End, content, dismiss)
        });
        div()
            .flex()
            .items_center()
            .gap(rems(0.5))
            .min_w_0()
            .child(
                div()
                    .relative()
                    .child(
                        Button::new("me-project", name)
                            .size(ButtonSize::Sm)
                            .ghost()
                            .icon(IconName::FolderSimple)
                            .icon_end(IconName::CaretUp)
                            .disabled(self.disabled || self.busy || loading)
                            .selected(self.open)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open = !this.open;
                                cx.notify();
                            })),
                    )
                    .children(menu),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .min_w_0()
                        .text_caption()
                        .text_color(c.text_danger)
                        .child(error),
                )
            })
    }
}

/// The assistant and project controls side by side, as the composer's
/// trailing row.
pub struct ChatControls {
    pub assistant: Entity<AssistantControl>,
    pub project: Entity<ProjectControl>,
}

impl ChatControls {
    pub fn set_disabled(&self, disabled: bool, cx: &mut App) {
        self.assistant
            .update(cx, |control, cx| control.set_disabled(disabled, cx));
        self.project
            .update(cx, |control, cx| control.set_disabled(disabled, cx));
    }
}

impl Render for ChatControls {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(rems(0.5))
            .min_w_0()
            .child(self.assistant.clone())
            .child(self.project.clone())
    }
}
