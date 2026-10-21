//! The owner's teams in the Agents page's side panel (buzz-app's `Teams`
//! and `TeamEditor`): each a name, instructions and members, given to
//! every member's prompt with a roster. Agents record nothing about their
//! teams.

use super::page::side_body;
use super::problem;
use crate::agents2::teams::{Team, TeamChange};
use crate::agents2::{Agent, Agents2};
use crate::ui::agents::form::field;
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, Checkbox, IconButton, IconButtonSize, IconName, Surface,
    TextArea, TextField, TypeStyled,
};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

/// The panel asks to close.
pub struct CloseTeams;

/// The editor of one team, or of a new one.
struct Editor {
    /// `None` for a new team.
    team: Option<Team>,
    name: Entity<InputState>,
    instructions: Entity<TextareaState>,
    members: Vec<String>,
    pending: bool,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

pub struct TeamsPanel {
    editing: Option<Editor>,
}

impl EventEmitter<CloseTeams> for TeamsPanel {}

/// The owner's agents here, which can be members.
fn saved(cx: &App) -> Vec<Agent> {
    Agents2::global(cx)
        .map(|service| service.read(cx).agents().to_vec())
        .unwrap_or_default()
}

fn teams(cx: &App) -> Vec<Team> {
    Agents2::global(cx)
        .map(|service| service.read(cx).teams().to_vec())
        .unwrap_or_default()
}

/// The panel's header: a title and Close.
fn header(
    title: &'static str,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(rems(0.5))
        .child(div().min_w_0().text_heading().child(title))
        .child(
            IconButton::new("teams-close", IconName::X)
                .size(IconButtonSize::Sm)
                .tooltip("Close")
                .on_click(on_close),
        )
}

impl TeamsPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        if let Some(service) = Agents2::global(cx) {
            cx.observe(&service, |this, _, cx| {
                // A team deleted elsewhere closes its editor.
                if let Some(Editor {
                    team: Some(team), ..
                }) = &this.editing
                    && !teams(cx).iter().any(|saved| saved.id == team.id)
                {
                    this.editing = None;
                }
                cx.notify();
            })
            .detach();
        }
        Self { editing: None }
    }

    fn edit(&mut self, team: Option<Team>, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(team.as_ref().map(|t| t.name.clone()).unwrap_or_default())
        });
        name.update(cx, |state, cx| state.focus(window, cx));
        let instructions = cx.new(|cx| TextareaState::new(window, cx));
        if let Some(team) = &team {
            instructions.update(cx, |state, cx| {
                state.set_value(team.instructions.clone(), window, cx)
            });
        }
        let subscriptions = vec![cx.subscribe_in(
            &name,
            window,
            |this, _, event: &InputEvent, _, cx| match event {
                InputEvent::PressEnter { .. } => this.save(cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        )];
        self.editing = Some(Editor {
            members: team.as_ref().map(|t| t.members.clone()).unwrap_or_default(),
            team,
            name,
            instructions,
            pending: false,
            error: None,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn done(&mut self, cx: &mut Context<Self>) {
        if self.editing.as_ref().is_some_and(|editor| editor.pending) {
            return;
        }
        self.editing = None;
        cx.notify();
    }

    /// Runs a team change; the editor closes when it succeeds.
    fn act(
        &mut self,
        work: impl FnOnce(&mut Agents2, &mut Context<Agents2>) -> Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let Some(service) = Agents2::global(cx) else {
            return;
        };
        let Some(editor) = &mut self.editing else {
            return;
        };
        if editor.pending {
            return;
        }
        editor.error = None;
        match service.update(cx, work) {
            Ok(()) => self.editing = None,
            Err(error) => editor.error = Some(error.into()),
        }
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editing else {
            return;
        };
        let name = editor.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            return;
        }
        let change = TeamChange {
            id: editor.team.as_ref().map(|team| team.id.clone()),
            name,
            instructions: editor.instructions.read(cx).value().to_string(),
            members: editor.members.clone(),
        };
        self.act(
            move |service, cx| service.save_team(change, cx).map(drop),
            cx,
        );
    }

    fn remove(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .editing
            .as_ref()
            .and_then(|editor| editor.team.as_ref())
            .map(|team| team.id.clone())
        else {
            return;
        };
        self.act(move |service, cx| service.remove_team(&id, cx), cx);
    }

    fn list(&self, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let agents = saved(cx);
        let teams = teams(cx);
        let name_of = |pubkey: &String| {
            agents
                .iter()
                .find(|agent| &agent.pubkey == pubkey)
                .map(|agent| agent.name.clone())
        };
        let rows = teams.iter().enumerate().map(|(index, team)| {
            let members = team
                .members
                .iter()
                .filter_map(name_of)
                .collect::<Vec<_>>()
                .join(", ");
            let edit = team.clone();
            div()
                .flex()
                .items_center()
                .gap(rems(0.75))
                .py(rems(0.5))
                .when(index > 0, |this| {
                    this.border_t_1().border_color(c.border_standard)
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().child(team.name.clone()))
                        .child(
                            div()
                                .truncate()
                                .text_caption()
                                .text_color(c.text_subtle)
                                .child(if members.is_empty() {
                                    "No members".to_string()
                                } else {
                                    members
                                }),
                        ),
                )
                .child(
                    Button::new(("team-edit", index), "Edit")
                        .size(ButtonSize::Sm)
                        .tooltip(format!("Edit {}", team.name))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.edit(Some(edit.clone()), window, cx)
                        })),
                )
        });
        side_body()
            .child(header(
                "Teams",
                cx.listener(|_, _, _, cx| cx.emit(CloseTeams)),
            ))
            .child(div().text_body_sm().text_color(c.text_subtle).child(
                "Each member is told who its teammates are, and follows the team's instructions after its own.",
            ))
            .map(|this| {
                if teams.is_empty() {
                    this.child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_subtle)
                            .child("No teams yet."),
                    )
                } else {
                    this.child(div().flex().flex_col().children(rows))
                }
            })
            .child(
                Button::new("team-new", "New team")
                    .prominent()
                    .full_width()
                    .icon(IconName::Plus)
                    .on_click(cx.listener(|this, _, window, cx| this.edit(None, window, cx))),
            )
    }

    fn editor(&self, editor: &Editor, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let agents = saved(cx);
        let pending = editor.pending;
        let existing = editor.team.is_some();
        let name_empty = editor.name.read(cx).value().trim().is_empty();
        let members = agents.iter().enumerate().map(|(index, agent)| {
            let pubkey = agent.pubkey.clone();
            let entity = cx.entity().downgrade();
            Checkbox::new(
                ("team-member", index),
                editor.members.contains(&agent.pubkey),
            )
            .label(agent.name.clone())
            .disabled(pending)
            .on_change(move |checked, _, cx| {
                let _ = entity.update(cx, |this, cx| {
                    if let Some(editor) = &mut this.editing {
                        editor.members.retain(|member| member != &pubkey);
                        if checked {
                            editor.members.push(pubkey.clone());
                        }
                        cx.notify();
                    }
                });
            })
        });
        side_body()
            .key_context("AgentsTeamEditor")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.done(cx);
                }
            }))
            .child(header(
                if existing { "Edit team" } else { "New team" },
                cx.listener(|this, _, _, cx| this.done(cx)),
            ))
            .child(field(
                "Name",
                None,
                TextField::new(&editor.name)
                    .surface(Surface::Floating)
                    .disabled(pending),
                &c,
            ))
            .child(field(
                "Instructions",
                Some("Every member follows these after its own instructions.".into()),
                TextArea::new(&editor.instructions)
                    .rows(5)
                    .surface(Surface::Floating)
                    .disabled(pending),
                &c,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(rems(0.5))
                    .child(
                        div()
                            .text_caption()
                            .text_color(c.text_subtle)
                            .child("Members"),
                    )
                    .map(|this| {
                        if agents.is_empty() {
                            this.child(
                                div()
                                    .text_body_sm()
                                    .text_color(c.text_subtle)
                                    .child("No agents yet."),
                            )
                        } else {
                            this.child(div().flex().flex_col().gap(rems(0.5)).children(members))
                        }
                    }),
            )
            .children(problem(editor.error.as_ref(), &c))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.5))
                    .child(
                        Button::new("team-save", if existing { "Save" } else { "Create team" })
                            .prominent()
                            .loading(pending)
                            .disabled(name_empty)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    )
                    .when(existing, |this| {
                        this.child(
                            Button::new("team-delete", "Delete team")
                                .destructive()
                                .disabled(pending)
                                .on_click(cx.listener(|this, _, _, cx| this.remove(cx))),
                        )
                    }),
            )
    }
}

impl Render for TeamsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.editing.take() {
            Some(editor) => {
                let view = self.editor(&editor, cx);
                self.editing = Some(editor);
                view
            }
            None => self.list(cx),
        }
    }
}
