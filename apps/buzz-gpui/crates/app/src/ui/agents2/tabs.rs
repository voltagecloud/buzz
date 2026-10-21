//! An Agents2 agent's sections as tabs, wherever they are shown (its Build
//! view and its profile panel): the type's own tabs, Settings first, then
//! the app's Attention and Memory tabs, which every type shares: the app is
//! what wakes the agent, and every type keeps its memory the same way
//! (buzz-app's `tabs.tsx` and `AgentTabs.tsx`).
//!
//! Settings opens with the app's Provider select (`AgentSettingsHost.tsx`):
//! choosing another provider previews what carries over and what resets,
//! then stages the destination's settings in the tab; its Save commits the
//! provider and those settings together.

use super::AttentionPanel;
use super::create::type_icon;
use super::memory::MemoryPanel;
use crate::agent_plugins::tabs::field;
use crate::agents2::{
    Agent, AgentConversation, AgentView, Agents2, CarryKey, MigrationOutcome, RegisteredType,
    RetypePreview, SettingsHost, registry::PendingSave, registry::TypeControl, type_of, types,
};
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_ui::{
    ActiveTheme, Button, Dialog, Icon, IconName, Menu, MenuItem, Select, Surface, Tab, TabVariant,
    Tabs, TypeStyled, tokens::space,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::Value;
use std::collections::HashMap;
use std::rc::Rc;

const SETTINGS: &str = "agent:settings";
const ATTENTION: &str = "agent:attention";
const MEMORY: &str = "agent:memory";
/// The id of a type's Settings tab, which the provider select heads.
const SETTINGS_TAB: &str = "settings";

/// A provider change chosen in Settings, until Save commits it.
struct Staged {
    key: SharedString,
    config: Value,
}

/// The preview shown before a provider change is staged.
struct Migration {
    target: RegisteredType,
    preview: RetypePreview,
    /// Settings had unsaved changes, which continuing discards.
    dirty: bool,
}

pub struct AgentTabs {
    pubkey: String,
    /// The chosen section; the first when unset.
    selected: Option<SharedString>,
    attention: Entity<AttentionPanel>,
    /// Built when first shown, so a closed tab reads no memory.
    memory: Option<Entity<MemoryPanel>>,
    /// Type tab views by type, staging and tab, built when first shown.
    views: HashMap<SharedString, AnyView>,
    /// The conversation the agent was opened from, which type tabs set
    /// apart.
    conversation: Option<AgentConversation>,
    staged: Option<Staged>,
    /// Counts staging changes, so a staged tab is built afresh.
    stage: u64,
    migration: Option<Migration>,
    provider_open: bool,
    provider_error: Option<String>,
    /// Settings' own Save, for an agent whose type has no Settings tab.
    saving: bool,
    error: Option<String>,
    _observe: Option<Subscription>,
}

impl AgentTabs {
    pub fn new(pubkey: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let attention = cx.new(|cx| AttentionPanel::new(pubkey.clone(), window, cx));
        let observe =
            Agents2::global(cx).map(|service| cx.observe(&service, |_, _, cx| cx.notify()));
        Self {
            pubkey,
            selected: None,
            attention,
            memory: None,
            views: HashMap::new(),
            conversation: None,
            staged: None,
            stage: 0,
            migration: None,
            provider_open: false,
            provider_error: None,
            saving: false,
            error: None,
            _observe: observe,
        }
    }

    /// Opened from another conversation, the type tabs start over with it,
    /// closing a transcript they showed.
    pub fn set_conversation(
        &mut self,
        conversation: Option<AgentConversation>,
        cx: &mut Context<Self>,
    ) {
        if conversation != self.conversation {
            self.conversation = conversation;
            self.views.clear();
            cx.notify();
        }
    }

    fn set_staged(&mut self, staged: Option<Staged>) {
        self.staged = staged;
        self.stage += 1;
        self.views.clear();
    }

    /// Whether the agent can move to another provider: a local one, as
    /// no hosted provider takes over a local key (`canRetype`).
    fn can_retype(agent: &Agent, cx: &App) -> bool {
        types(cx)
            .iter()
            .any(|kind| kind.key != agent.agent_type && kind.kind.remote().is_none())
    }

    /// What Settings is framed with: the Provider select, and the staged
    /// change its Save commits.
    fn settings_host(&self, agent: &Agent, cx: &mut Context<Self>) -> SettingsHost {
        let tabs = cx.entity().downgrade();
        let type_control: Option<TypeControl> = Self::can_retype(agent, cx).then(|| {
            let tabs = tabs.clone();
            Rc::new(move |dirty: bool, window: &mut Window, cx: &mut App| {
                tabs.update(cx, |this, cx| this.provider_field(dirty, window, cx))
                    .unwrap_or_else(|_| div().into_any_element())
            }) as TypeControl
        });
        let pending = self.staged.as_ref().map(|staged| {
            let (pubkey, key) = (self.pubkey.clone(), staged.key.to_string());
            Rc::new(move |config: Value, cx: &mut App| {
                let Some(service) = Agents2::global(cx) else {
                    return Task::ready(Err("Agents is not running".to_string()));
                };
                let retype = service.update(cx, |service, cx| {
                    service.retype(&pubkey, &key, Some(config), cx)
                });
                let tabs = tabs.clone();
                cx.spawn(async move |cx| {
                    retype.await?;
                    let _ = tabs.update(cx, |this, cx| {
                        this.set_staged(None);
                        cx.notify();
                    });
                    Ok(())
                })
            }) as PendingSave
        });
        SettingsHost {
            type_control,
            pending,
        }
    }

    /// Previews moving to `key`, or unstages a change back to its own
    /// type.
    fn choose(&mut self, key: SharedString, dirty: bool, cx: &mut Context<Self>) {
        self.provider_open = false;
        let Some(agent) = current(&self.pubkey, cx) else {
            return;
        };
        let selected = self
            .staged
            .as_ref()
            .map_or(agent.agent_type.as_str(), |staged| staged.key.as_ref());
        if key.as_ref() == selected {
            return;
        }
        if key.as_ref() == agent.agent_type {
            self.set_staged(None);
            cx.notify();
            return;
        }
        let Some(target) = types(cx).into_iter().find(|kind| kind.key == key) else {
            return;
        };
        self.provider_error = None;
        let preview = Agents2::global(cx)
            .ok_or_else(|| "Agents is not running".to_string())
            .and_then(|service| {
                service.update(cx, |service, cx| {
                    service.preview_retype(&self.pubkey, &key, cx)
                })
            });
        match preview {
            Ok(preview) => {
                self.migration = Some(Migration {
                    target,
                    preview,
                    dirty,
                })
            }
            Err(error) => self.provider_error = Some(error),
        }
        cx.notify();
    }

    /// Stages the previewed change: Settings shows the destination's
    /// settings until Save.
    fn stage(&mut self, cx: &mut Context<Self>) {
        let Some(migration) = self.migration.take() else {
            return;
        };
        self.provider_error = None;
        self.set_staged(Some(Staged {
            key: migration.target.key,
            config: migration.preview.config,
        }));
        cx.notify();
    }

    /// The Provider field (`TypeField`): the chosen provider, a menu of the
    /// others when there are any, and the preview before a change.
    fn provider_field(
        &mut self,
        dirty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let Some(agent) = current(&self.pubkey, cx) else {
            return div().into_any_element();
        };
        // Local providers only, besides the agent's own (`!type.remote`).
        let available: Vec<_> = types(cx)
            .into_iter()
            .filter(|kind| kind.key == agent.agent_type || kind.kind.remote().is_none())
            .collect();
        let others = available.iter().any(|kind| kind.key != agent.agent_type);
        let selected = self
            .staged
            .as_ref()
            .map_or(agent.agent_type.clone(), |staged| staged.key.to_string());
        let selected_type = available
            .iter()
            .find(|kind| kind.key == selected)
            .cloned()
            .or_else(|| type_of(&agent.agent_type, cx));
        let title: SharedString = selected_type
            .as_ref()
            .map_or("Type unavailable".into(), |kind| kind.kind.title());
        let control: AnyElement = if others {
            let open = self.provider_open;
            let select = Select::new("agent-provider")
                .value(title.clone())
                .leading(type_icon(selected_type.as_ref(), 20., cx))
                .surface(Surface::Floating)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.provider_open = !this.provider_open;
                    this.provider_error = None;
                    cx.notify();
                }));
            let mut menu = Menu::new();
            for (index, kind) in available.iter().enumerate() {
                let key = kind.key.clone();
                menu = menu.child(
                    MenuItem::new(("agent-provider", index), kind.kind.title())
                        .leading(type_icon(Some(kind), 20., cx))
                        .checked(kind.key == selected)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.choose(key.clone(), dirty, cx)),
                        ),
                );
            }
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.provider_open = false;
                    cx.notify();
                });
            });
            overlay::select_menu(
                select,
                open,
                Align::Start,
                Some(rems(16.)),
                || menu.into_any_element(),
                dismiss,
                window,
            )
            .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .gap(space::S2)
                .min_w_0()
                .child(type_icon(selected_type.as_ref(), 20., cx))
                .child(title)
                .into_any_element()
        };
        let dialog = self
            .migration
            .as_ref()
            .map(|migration| self.migration_dialog(&agent, migration, window, cx));
        div()
            .flex()
            .flex_col()
            .gap(space::S1)
            .child(field("Provider", control, None, &c))
            .when_some(
                self.provider_error
                    .clone()
                    .filter(|_| self.migration.is_none()),
                |this, error| {
                    this.child(div().text_body_sm().text_color(c.text_danger).child(error))
                },
            )
            .children(dialog)
            .into_any_element()
    }

    /// "Move {name} to {provider}?" with what carries over and what
    /// resets; Continue stages the change.
    fn migration_dialog(
        &self,
        agent: &Agent,
        migration: &Migration,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let source = type_of(&agent.agent_type, cx);
        let side = |kind: Option<&RegisteredType>, title: SharedString, cx: &App| {
            div()
                .flex()
                .items_center()
                .gap(space::S2)
                .child(type_icon(kind, 24., cx))
                .child(title)
        };
        let warning = source
            .as_ref()
            .and_then(|source| source.kind.migration_warning());
        let preview = &migration.preview;
        let carry: Vec<CarryKey> = CarryKey::ALL
            .into_iter()
            .filter(|key| preview.moved.contains(key) || preview.dropped.contains(key))
            .collect();
        let row = |label: &'static str, outcome: &'static str, color: Hsla| {
            div()
                .flex()
                .justify_between()
                .gap(space::S3)
                .child(div().text_body().child(label))
                .child(div().text_body_sm().text_color(color).child(outcome))
        };
        let mut rows = Vec::new();
        for key in carry {
            rows.push(if preview.dropped.contains(&key) {
                row(key.label(), "Dropped", c.text_danger)
            } else {
                row(key.label(), "Moves", c.text_subtle)
            });
        }
        for setting in &preview.settings {
            rows.push(match setting.outcome {
                MigrationOutcome::Reset => row(setting.label, "Resets", c.text_warning),
                MigrationOutcome::Dropped => row(setting.label, "Dropped", c.text_danger),
            });
        }
        let body = div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S3)
                    .text_body()
                    .child(side(
                        source.as_ref(),
                        source
                            .as_ref()
                            .map_or("Unavailable type".into(), |kind| kind.kind.title()),
                        cx,
                    ))
                    .child(
                        Icon::new(IconName::ArrowRight)
                            .size(16.)
                            .color(c.text_standard),
                    )
                    .child(side(
                        Some(&migration.target),
                        migration.target.kind.title(),
                        cx,
                    )),
            )
            .child(
                div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(intro(warning)),
            )
            .when(migration.dirty, |this| {
                this.child(
                    div()
                        .text_body_sm()
                        .text_color(c.text_warning)
                        .child("Unsaved Settings changes will be discarded when you continue."),
                )
            })
            .when(!rows.is_empty(), |this| {
                this.child(div().flex().flex_col().gap(space::S2).children(rows))
            });
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.migration = None;
                this.provider_error = None;
                cx.notify();
            });
        });
        let dialog = Dialog::new(
            "agent-migration",
            format!("Move {} to {}?", agent.name, migration.target.kind.title()),
        )
        .compact_header_gap()
        .on_close(cx.listener(|this, _, _, cx| {
            this.migration = None;
            this.provider_error = None;
            cx.notify();
        }))
        .child(body)
        .footer(
            Button::new("agent-migration-cancel", "Cancel")
                .surface(Surface::Floating)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.migration = None;
                    cx.notify();
                })),
        )
        .footer(
            Button::new("agent-migration-continue", "Continue")
                .prominent()
                .on_click(cx.listener(|this, _, _, cx| this.stage(cx))),
        );
        overlay::modal(window, dialog, Some(dismiss)).into_any_element()
    }

    /// Settings for a type with no Settings tab of its own, such as one no
    /// longer available (`HostSettings`): the Provider select and Save.
    fn host_settings(
        &mut self,
        host: SettingsHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        // Built here rather than through `type_control`, which updates this
        // view and so cannot run while it renders.
        let control = host
            .type_control
            .is_some()
            .then(|| self.provider_field(false, window, cx));
        let pending = host.pending.clone();
        div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .children(control)
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_body_sm().text_color(c.text_danger).child(error))
            })
            .child(
                div().flex().child(
                    Button::new("agent-host-settings-save", "Save")
                        .prominent()
                        .loading(self.saving)
                        .disabled(pending.is_none() || self.saving)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let Some(pending) = pending.clone() else {
                                return;
                            };
                            let Some(config) = this.staged.as_ref().map(|s| s.config.clone())
                            else {
                                return;
                            };
                            this.saving = true;
                            this.error = None;
                            let save = pending(config, cx);
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
                        })),
                ),
            )
            .into_any_element()
    }
}

/// The preview's note: the change waits for Save, and what leaving the old
/// provider stops.
fn intro(warning: Option<&str>) -> String {
    let mut text = "The provider changes when you save settings. Your agent will continue the conversation in Buzz.".to_string();
    if let Some(warning) = warning {
        text.push(' ');
        text.push_str(warning);
    }
    text
}

fn current(pubkey: &str, cx: &App) -> Option<Agent> {
    Agents2::global(cx).and_then(|service| service.read(cx).find(pubkey).cloned())
}

impl Render for AgentTabs {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(agent) = current(&self.pubkey, cx) else {
            return div();
        };
        // A staged change that no longer applies (its type went away, or
        // the agent now has it) is dropped.
        if let Some(staged) = &self.staged
            && (staged.key == agent.agent_type || type_of(&staged.key, cx).is_none())
        {
            self.set_staged(None);
        }
        let host = self.settings_host(&agent, cx);
        // With a change staged, the destination's tabs edit its settings.
        let (kind, shown) = match &self.staged {
            Some(staged) => (
                type_of(&staged.key, cx),
                Agent {
                    agent_type: staged.key.to_string(),
                    config: staged.config.clone(),
                    ..agent.clone()
                },
            ),
            None => (type_of(&agent.agent_type, cx), agent.clone()),
        };
        let mut type_tabs = kind
            .as_ref()
            .map(|kind| kind.kind.tabs())
            .unwrap_or_default();
        // Settings comes first.
        if let Some(index) = type_tabs.iter().position(|tab| tab.id == SETTINGS_TAB) {
            let settings = type_tabs.remove(index);
            type_tabs.insert(0, settings);
        }
        let has_settings = type_tabs.first().is_some_and(|tab| tab.id == SETTINGS_TAB);
        let section = |id: &SharedString| -> SharedString {
            if id.as_ref() == SETTINGS_TAB {
                SETTINGS.into()
            } else {
                format!("agent:type:{id}").into()
            }
        };
        let mut sections: Vec<(SharedString, SharedString)> = type_tabs
            .iter()
            .map(|tab| (section(&tab.id), tab.title.clone()))
            .collect();
        let host_tab = !has_settings && host.type_control.is_some();
        if host_tab {
            sections.insert(0, (SETTINGS.into(), "Settings".into()));
        }
        sections.push((ATTENTION.into(), "Attention".into()));
        sections.push((MEMORY.into(), "Memory".into()));
        // A tab that went away (its type was replaced) falls back to the last.
        let selected = self
            .selected
            .clone()
            .filter(|value| sections.iter().any(|(v, _)| v == value))
            .unwrap_or_else(|| {
                if self.selected.is_some() {
                    sections.last().expect("Memory is always there").0.clone()
                } else {
                    sections[0].0.clone()
                }
            });
        let mut tabs = Tabs::new(TabVariant::Panel);
        for (value, label) in &sections {
            let value = value.clone();
            tabs = tabs.tab(
                Tab::new(
                    SharedString::from(format!("{}-{value}", self.pubkey)),
                    label.clone(),
                )
                .selected(value == selected)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected = Some(value.clone());
                    cx.notify();
                })),
            );
        }
        // A type's tabs close while Buzz's own section shows.
        let own_section = selected.as_ref() == ATTENTION
            || selected.as_ref() == MEMORY
            || (host_tab && selected.as_ref() == SETTINGS);
        if own_section {
            self.views.clear();
        }
        let panel: AnyElement = if selected.as_ref() == ATTENTION {
            self.attention.clone().into_any_element()
        } else if selected.as_ref() == MEMORY {
            let pubkey = self.pubkey.clone();
            self.memory
                .get_or_insert_with(|| cx.new(|cx| MemoryPanel::new(pubkey, cx)))
                .clone()
                .into_any_element()
        } else if host_tab && selected.as_ref() == SETTINGS {
            self.host_settings(host, window, cx)
        } else {
            let tab = type_tabs.iter().find(|tab| section(&tab.id) == selected);
            match tab {
                Some(tab) => {
                    let key: SharedString =
                        format!("{}#{}:{}", shown.agent_type, self.stage, tab.id).into();
                    // Only the tab shown stays open, as an unmounted panel
                    // closes (`Tabs.Panel`).
                    self.views.retain(|kept, _| *kept == key);
                    let view = match self.views.get(&key) {
                        // A kept tab shows the agent as it is now.
                        Some(view) => {
                            crate::external_plugins::ExternalView::agent_changed(view, &shown, cx);
                            view.clone()
                        }
                        None => {
                            // Every tab saves through a staged change; only
                            // Settings has the Provider field.
                            let settings_host = if tab.id == SETTINGS_TAB {
                                Some(host)
                            } else {
                                Some(crate::agents2::registry::SettingsHost {
                                    type_control: None,
                                    pending: host.pending.clone(),
                                })
                            };
                            let view = (tab.build)(
                                AgentView {
                                    agent: shown.clone(),
                                    conversation: self.conversation.clone(),
                                    settings_host,
                                },
                                window,
                                cx,
                            );
                            self.views.insert(key, view.clone());
                            view
                        }
                    };
                    view.into_any_element()
                }
                None => div().into_any_element(),
            }
        };
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .child(tabs)
            .child(div().pt(rems(1.)).child(panel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn the_preview_says_the_change_waits_for_save_and_what_leaving_stops() {
        assert_eq!(
            intro(None),
            "The provider changes when you save settings. Your agent will continue the conversation in Buzz."
        );
        assert!(
            intro(Some("Any commands Codex left running will stop."))
                .ends_with("in Buzz. Any commands Codex left running will stop.")
        );
    }
}
