//! The Agents page's Import tab (`ImportAgents` in `AgentsPage.tsx`):
//! identities earlier agent systems saved on the hosts, each imported with
//! its key, name and picture as an agent of the chosen type, which may
//! adopt its instructions and workspace.

use super::card::{self, AgentCard};
use super::problem;
use crate::agents2::{Agents2, Portable, Portables, types};
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_host::api::PortSource;
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, Menu, MenuItem, Select, Surface, Tooltip, TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::HashMap;
use std::rc::Rc;

/// Where each was saved (`SOURCES`).
fn source_label(source: PortSource) -> &'static str {
    match source {
        PortSource::Harness => "Buzz 1.0 harness agent",
        PortSource::Installed => "Buzz 0.X",
        PortSource::Development => "Buzz 0.X dev",
    }
}

pub enum BringOverEvent {
    /// One was imported: the page shows Your agents.
    Imported,
}

pub struct BringOver {
    found: Option<Portables>,
    kind: Option<SharedString>,
    kind_open: bool,
    /// The agent being imported, by key.
    pending: Option<String>,
    error: Option<SharedString>,
    _load: Task<()>,
}

impl EventEmitter<BringOverEvent> for BringOver {}

impl BringOver {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let load = match Agents2::global(cx) {
            Some(service) => {
                let task = service.update(cx, |service, cx| service.portable(cx));
                cx.spawn(async move |this, cx| {
                    let found = task.await;
                    let _ = this.update(cx, |this, cx| {
                        match found {
                            Ok(found) => this.found = Some(found),
                            Err(error) => this.error = Some(error.into()),
                        }
                        cx.notify();
                    });
                })
            }
            None => Task::ready(()),
        };
        Self {
            found: None,
            kind: types(cx).first().map(|kind| kind.key.clone()),
            kind_open: false,
            pending: None,
            error: None,
            _load: load,
        }
    }

    fn port(&mut self, agent: Portable, cx: &mut Context<Self>) {
        let all = types(cx);
        let chosen = self
            .kind
            .as_ref()
            .and_then(|key| all.iter().find(|kind| &kind.key == key))
            .or(all.first())
            .cloned();
        let (Some(chosen), Some(service)) = (chosen, Agents2::global(cx)) else {
            return;
        };
        if self.pending.is_some() {
            return;
        }
        self.pending = Some(agent.agent.pubkey.clone());
        self.error = None;
        let task = service.update(cx, |service, cx| service.port(&agent, &chosen.key, cx));
        let pubkey = agent.agent.pubkey;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.pending = None;
                match result {
                    Ok(_) => {
                        if let Some(found) = &mut this.found {
                            found.agents.retain(|item| item.agent.pubkey != pubkey);
                        }
                        cx.emit(BringOverEvent::Imported);
                    }
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The "Import as" select.
    fn kind_select(
        &self,
        chosen: Option<&SharedString>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let all = types(cx);
        let label = all
            .iter()
            .find(|kind| Some(&kind.key) == chosen)
            .map(|kind| kind.kind.title());
        let mut menu = Menu::new();
        for (index, kind) in all.iter().enumerate() {
            let key = kind.key.clone();
            menu = menu.child(
                MenuItem::new(("import-as", index), kind.kind.title())
                    .checked(Some(&kind.key) == chosen)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kind = Some(key.clone());
                        this.kind_open = false;
                        cx.notify();
                    })),
            );
        }
        let weak = cx.entity().downgrade();
        let dismiss: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.kind_open = false;
                cx.notify();
            });
        });
        let select = Select::new("import-as-select")
            .when_some(label, |this, label| this.value(label))
            .surface(Surface::Floating)
            .disabled(self.pending.is_some())
            .on_click(cx.listener(|this, _, _, cx| {
                this.kind_open = !this.kind_open;
                cx.notify();
            }));
        div()
            .flex()
            .flex_col()
            .gap(rems(0.5))
            .w(rems(16.))
            .child(div().text_label_sm().child("Import as"))
            .child(overlay::select_menu(
                select,
                self.kind_open,
                Align::Start,
                Some(rems(16.)),
                || menu.into_any_element(),
                dismiss,
                window,
            ))
    }
}

impl Render for BringOver {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let all = types(cx);
        if all.is_empty() {
            return div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child("Enable an agent type plugin to import agents.");
        }
        let chosen = self
            .kind
            .as_ref()
            .and_then(|key| all.iter().find(|kind| &kind.key == key))
            .or(all.first())
            .map(|kind| kind.key.clone());
        let busy = self.pending.is_some();
        let adopts = all
            .iter()
            .find(|kind| Some(&kind.key) == chosen.as_ref())
            .is_some_and(|kind| kind.kind.adopts());
        let keeps = if adopts {
            "Keeps each agent's key, name, picture and instructions in this community. Its \
             other settings stay behind."
        } else {
            "Keeps each agent's key, name and picture in this community. Its instructions and \
             settings stay behind."
        };
        let list: AnyElement = match &self.found {
            None if self.error.is_none() => div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child("Looking for agents…")
                .into_any_element(),
            None => div().into_any_element(),
            Some(found) if found.agents.is_empty() => div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child("No other agents found to import.")
                .into_any_element(),
            Some(found) => {
                let labels = crate::ui::keys::public_key_labels(
                    found.agents.iter().map(|a| &a.agent.pubkey),
                );
                let mut named: HashMap<&str, usize> = HashMap::new();
                for item in &found.agents {
                    *named.entry(item.agent.name.as_str()).or_default() += 1;
                }
                let hosts = found
                    .agents
                    .iter()
                    .map(|a| a.host.entity_id())
                    .collect::<std::collections::HashSet<_>>()
                    .len();
                let cards = found.agents.iter().enumerate().map(|(index, item)| {
                    let agent = &item.agent;
                    let mut detail = vec![source_label(agent.source).to_string()];
                    // Its key tells it apart only from one of the same name.
                    if named.get(agent.name.as_str()) != Some(&1) {
                        detail.extend(labels.get(&agent.pubkey.to_ascii_lowercase()).cloned());
                    }
                    // Port-only: which of several hosts saved it.
                    if hosts > 1 {
                        detail.push(item.host.read(cx).label.to_string());
                    }
                    let tip = [
                        crate::ui::keys::npub(&agent.pubkey),
                        agent.relay.trim_start_matches("wss://").to_string(),
                    ]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                    let pending = self.pending.as_deref() == Some(agent.pubkey.as_str());
                    let port = item.clone();
                    AgentCard::new(
                        ("import-agent", index),
                        agent.name.clone(),
                        agent.picture.clone().filter(|p| !p.is_empty()),
                        agent.relay.clone(),
                    )
                    .detail(
                        div()
                            .id(("import-detail", index))
                            .tooltip(Tooltip::text(tip))
                            .child(detail.join(" · ")),
                    )
                    .action(
                        Button::new(("import", index), "Import")
                            .size(ButtonSize::Sm)
                            .outline()
                            .tooltip(format!("Import {}", agent.name))
                            .loading(pending)
                            .disabled(chosen.is_none() || (busy && !pending))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.port(port.clone(), cx)),
                            ),
                    )
                });
                card::grid(window).children(cards).into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(rems(1.))
            .child(
                div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(format!(
                        "{keeps} Stop it where it runs now first, or both will answer."
                    )),
            )
            .when(all.len() > 1, |this| {
                this.child(self.kind_select(chosen.as_ref(), window, cx))
            })
            .children(problem(self.error.as_ref(), &c))
            .children(self.found.iter().flat_map(|found| {
                found.warnings.iter().map(|warning| {
                    div()
                        .text_body_sm()
                        .text_color(c.text_subtle)
                        .child(warning.clone())
                })
            }))
            .child(list)
    }
}
