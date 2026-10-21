//! Settings → Channel templates (`channels/ChannelTemplateSettings.tsx`):
//! templates a new channel can start from, each its agents, Agents teams
//! and starting Canvas, kept on this device.

use super::cards::{self, CardPlacement, SettingsCard};
use super::{button, empty_card, form, header, preference_row, rows_group, section};
use crate::agents2::{Agent, Agents2, teams::Team};
use crate::channel_templates::{ChannelTemplate, ChannelTemplates, TemplateChange};
use crate::ui::overlay::{self, Dismiss};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Button, Checkbox, Dialog, IconName, Surface, TextArea,
    TextField, TypeStyled,
};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// Registers the "Channel templates" card under the community, after
/// Messages (`channels/index.tsx`).
pub fn init(cx: &mut App) {
    cards::register(
        SettingsCard {
            key: "buzz.channels/templates",
            title: "Channel templates",
            icon: Some(IconName::Copy),
            placement: CardPlacement::Community,
            visibility: None,
            build: super::cards::build(|window, cx| {
                cx.new(|cx| TemplateSettings::new(window, cx)).into()
            }),
        },
        cx,
    );
}

/// The template being made (`id: None`) or edited.
struct Editor {
    id: Option<String>,
    name: Entity<InputState>,
    canvas: Entity<TextareaState>,
    agents: Vec<String>,
    teams: Vec<String>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

pub struct TemplateSettings {
    editor: Option<Editor>,
    _observers: Vec<Subscription>,
}

impl TemplateSettings {
    fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut observers = Vec::new();
        if let Some(templates) = ChannelTemplates::global(cx) {
            observers.push(cx.observe(&templates, |_, _, cx| cx.notify()));
        }
        if let Some(agents) = Agents2::global(cx) {
            observers.push(cx.observe(&agents, |_, _, cx| cx.notify()));
        }
        Self {
            editor: None,
            _observers: observers,
        }
    }

    fn edit(
        &mut self,
        template: Option<ChannelTemplate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let template = template.unwrap_or(ChannelTemplate {
            id: String::new(),
            name: String::new(),
            teams: Vec::new(),
            agents: Vec::new(),
            canvas: String::new(),
        });
        let name = cx.new(|cx| InputState::new(window, cx));
        name.update(cx, |state, cx| {
            state.set_value(template.name.clone(), window, cx);
            state.focus(window, cx);
        });
        let canvas = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("# Goal\n\n# Scope\n\n# Success criteria")
                .rows(8)
        });
        canvas.update(cx, |state, cx| {
            state.set_value(template.canvas.clone(), window, cx)
        });
        let subscriptions = vec![cx.subscribe_in(
            &name,
            window,
            |this, _, event: &InputEvent, _, cx| match event {
                InputEvent::PressEnter { .. } => this.save(cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        )];
        self.editor = Some(Editor {
            id: (!template.id.is_empty()).then_some(template.id),
            name,
            canvas,
            // Agents saved from another community stay chosen.
            agents: template.agents,
            teams: template.teams,
            error: None,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        cx.notify();
    }

    /// Runs `work` on the templates; the editor closes once it succeeds
    /// and shows the error otherwise.
    fn act(
        &mut self,
        work: impl FnOnce(&mut ChannelTemplates, &mut Context<ChannelTemplates>) -> Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let Some(templates) = ChannelTemplates::global(cx) else {
            return;
        };
        match templates.update(cx, work) {
            Ok(()) => self.editor = None,
            Err(error) => {
                if let Some(editor) = &mut self.editor {
                    editor.error = Some(error.into());
                }
            }
        }
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let name = editor.name.read(cx).value().to_string();
        if name.trim().is_empty() {
            return;
        }
        let change = TemplateChange {
            id: editor.id.clone(),
            name,
            teams: editor.teams.clone(),
            agents: editor.agents.clone(),
            canvas: editor.canvas.read(cx).value().to_string(),
        };
        self.act(
            move |templates, cx| templates.save(change, cx).map(|_| ()),
            cx,
        );
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.editor.as_ref().and_then(|editor| editor.id.clone()) else {
            return;
        };
        self.act(move |templates, cx| templates.remove(&id, cx), cx);
    }

    fn dialog(
        &self,
        editor: &Editor,
        agents: &[Agent],
        teams: &[Team],
        cx: &mut Context<Self>,
    ) -> Dialog {
        let c = *cx.colors();
        let named = !editor.name.read(cx).value().trim().is_empty();
        let agent_rows: Vec<AnyElement> = if agents.is_empty() {
            vec![form::note("No agents yet.", cx).into_any_element()]
        } else {
            agents
                .iter()
                .map(|agent| {
                    let pubkey = agent.pubkey.clone();
                    let weak = cx.entity().downgrade();
                    Checkbox::new(
                        SharedString::from(format!("template-agent-{}", agent.pubkey)),
                        editor.agents.contains(&agent.pubkey),
                    )
                    .label_element(agent_label(agent, cx))
                    .surface(Surface::Floating)
                    .on_change(move |checked, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            if let Some(editor) = &mut this.editor {
                                toggle(&mut editor.agents, &pubkey, checked);
                            }
                            cx.notify();
                        });
                    })
                    .into_any_element()
                })
                .collect()
        };
        let team_rows: Vec<AnyElement> = if teams.is_empty() {
            vec![form::note("Make teams on the Agents page.", cx).into_any_element()]
        } else {
            teams
                .iter()
                .map(|team| {
                    let id = team.id.clone();
                    let weak = cx.entity().downgrade();
                    Checkbox::new(
                        SharedString::from(format!("template-team-{}", team.id)),
                        editor.teams.contains(&team.id),
                    )
                    .label(team.name.clone())
                    .surface(Surface::Floating)
                    .on_change(move |checked, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            if let Some(editor) = &mut this.editor {
                                toggle(&mut editor.teams, &id, checked);
                            }
                            cx.notify();
                        });
                    })
                    .into_any_element()
                })
                .collect()
        };
        let choices = |legend: &'static str, rows: Vec<AnyElement>| {
            div()
                .flex()
                .flex_col()
                .gap(rems(0.5))
                .min_w_0()
                .child(div().text_label_sm().child(legend))
                .children(rows)
        };
        let field =
            |label: &'static str, description: Option<&'static str>, control: AnyElement| {
                form::field(
                    label,
                    description.map(Into::into),
                    None,
                    control,
                    form::FieldStyle::default(),
                    cx,
                )
            };
        let mut dialog = Dialog::new(
            "channel-template",
            if editor.id.is_some() {
                "Edit template"
            } else {
                "New template"
            },
        )
        .on_close(cx.listener(|this, _, _, cx| this.close(cx)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(rems(1.))
                .child(field(
                    "Name",
                    None,
                    TextField::new(&editor.name)
                        .surface(Surface::Floating)
                        .into_any_element(),
                ))
                .child(choices("Agents", agent_rows))
                .child(choices("Teams", team_rows))
                .child(field(
                    "Starting Canvas",
                    Some("Markdown, copied into each new channel made from this template."),
                    TextArea::new(&editor.canvas)
                        .surface(Surface::Floating)
                        .into_any_element(),
                ))
                .children(
                    editor
                        .error
                        .clone()
                        .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
                ),
        );
        if editor.id.is_some() {
            dialog = dialog.footer(
                Button::new("template-delete", "Delete")
                    .destructive()
                    .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
            );
        }
        dialog.footer(
            Button::new("template-save", "Save template")
                .prominent()
                .disabled(!named)
                .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
        )
    }
}

impl Render for TemplateSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let templates: Vec<ChannelTemplate> = ChannelTemplates::global(cx)
            .map(|templates| templates.read(cx).templates().to_vec())
            .unwrap_or_default();
        let (agents, teams) = Agents2::global(cx)
            .map(|agents| {
                let agents = agents.read(cx);
                (agents.agents().to_vec(), agents.teams().to_vec())
            })
            .unwrap_or_default();
        // A template deleted meanwhile is no longer edited.
        if let Some(Editor { id: Some(id), .. }) = &self.editor
            && !templates.iter().any(|item| &item.id == id)
        {
            self.editor = None;
        }
        let mut children = vec![header("Channel templates", None, cx).into_any_element()];
        if templates.is_empty() {
            children.push(
                empty_card(
                    IconName::Copy,
                    c.text_subtle,
                    "No channel templates yet",
                    div().text_body_sm().text_color(c.text_subtle).child(
                        "Start new channels with your agents, teams and a Canvas already \
                             in place.",
                    ),
                    Some(
                        button("template-new", "New template")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.edit(None, window, cx)),
                            )
                            .into_any_element(),
                    ),
                    cx,
                )
                .into_any_element(),
            );
        } else {
            let names: Vec<(String, String)> = agents
                .iter()
                .map(|agent| (agent.pubkey.clone(), agent.name.clone()))
                .collect();
            let rows = templates
                .iter()
                .map(|template| {
                    let edited = template.clone();
                    preference_row(
                        None,
                        template.name.clone(),
                        Some(summary(template, &names, &teams).into_any_element()),
                        vec![
                            button(
                                SharedString::from(format!("template-edit-{}", template.id)),
                                "Edit",
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit(Some(edited.clone()), window, cx)
                            }))
                            .into_any_element(),
                        ],
                        cx,
                    )
                    .into_any_element()
                })
                .collect();
            children.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(1.))
                    .child(rows_group(rows, cx))
                    .child(
                        div().flex().child(
                            button("template-new", "New template")
                                .icon(IconName::Plus)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.edit(None, window, cx)),
                                ),
                        ),
                    )
                    .into_any_element(),
            );
        }
        if let Some(editor) = &self.editor {
            let dialog = self.dialog(editor, &agents, &teams, cx);
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| this.close(cx));
            });
            children.push(overlay::modal(window, dialog, Some(dismiss)).into_any_element());
        }
        section(children)
    }
}

/// An agent's avatar beside its name (`AgentLabel`): the agent squircle,
/// initials until a picture loads.
fn agent_label(agent: &Agent, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .gap(rems(0.5))
        .child(
            Avatar::new(agent.name.clone())
                .size(AvatarSize::Small)
                .squircle()
                .when_some(
                    crate::ui::agents2::agent_picture(agent, cx),
                    |avatar, picture| avatar.image(picture),
                ),
        )
        .child(agent.name.clone())
}

/// Adds `item` last when checked, removes it otherwise.
fn toggle(list: &mut Vec<String>, item: &str, checked: bool) {
    list.retain(|saved| saved != item);
    if checked {
        list.push(item.to_owned());
    }
}

/// A template's row subtitle: its agents' and teams' names that still
/// exist here, then "Canvas" if it starts one, or "Empty".
fn summary(template: &ChannelTemplate, agents: &[(String, String)], teams: &[Team]) -> String {
    let names: Vec<&str> = template
        .agents
        .iter()
        .filter_map(|pubkey| {
            agents
                .iter()
                .find(|(key, _)| key == pubkey)
                .map(|(_, name)| name.as_str())
        })
        .chain(template.teams.iter().filter_map(|id| {
            teams
                .iter()
                .find(|team| &team.id == id)
                .map(|team| team.name.as_str())
        }))
        .collect();
    let parts: Vec<String> = [
        names.join(", "),
        if template.canvas.trim().is_empty() {
            String::new()
        } else {
            "Canvas".into()
        },
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect();
    if parts.is_empty() {
        "Empty".into()
    } else {
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn team(id: &str, name: &str) -> Team {
        Team {
            id: id.into(),
            name: name.into(),
            instructions: String::new(),
            members: Vec::new(),
        }
    }

    #[test]
    fn summary_lists_agents_then_teams_and_the_canvas() {
        let agents = vec![
            ("a".repeat(64), "Ada".to_owned()),
            ("b".repeat(64), "Grace".to_owned()),
        ];
        let teams = [team("t1", "Core"), team("t2", "Docs")];
        let mut template = ChannelTemplate {
            id: "x".into(),
            name: "Launch".into(),
            teams: vec!["t2".into(), "gone".into()],
            agents: vec!["a".repeat(64), "elsewhere".into()],
            canvas: "# Goal".into(),
        };
        assert_eq!(summary(&template, &agents, &teams), "Ada, Docs · Canvas");
        template.canvas = " \n".into();
        assert_eq!(summary(&template, &agents, &teams), "Ada, Docs");
        template.agents.clear();
        template.teams.clear();
        assert_eq!(summary(&template, &agents, &teams), "Empty");
        template.canvas = "x".into();
        assert_eq!(summary(&template, &agents, &teams), "Canvas");
    }

    #[test]
    fn toggling_appends_in_the_order_chosen() {
        let mut teams = vec!["t2".to_owned()];
        toggle(&mut teams, "t1", true);
        assert_eq!(teams, ["t2", "t1"]);
        toggle(&mut teams, "t2", false);
        assert_eq!(teams, ["t1"]);
        toggle(&mut teams, "t1", true);
        assert_eq!(teams, ["t1"]);
    }
}
