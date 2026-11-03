//! First-class team and multi-agent migration, using the existing type editor
//! and single-agent operation rather than importing copies of identities.

use super::migration_controls::{Button, Checkbox};
use crate::agent_plugins::remote_workspace::config::Config;
use crate::agents2::remote_migration::{
    self as migration, FileJournal, Journal, Plan, Scope, State, Step, TARGET,
};
use crate::agents2::{AgentView, Agents2, SettingsHost, type_of};
use crate::ui::overlay;
use buzz_ui::{ActiveTheme, ButtonSize, Dialog, DialogSize, Surface, TypeStyled};
use gpui_kit::base::FocusTrapElement;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::BTreeSet;
use std::rc::Rc;

/// The owner closed the migration dialog.
pub struct CloseMigration;

/// One dialog can select a saved team, arbitrary existing agents, and a shared
/// configuration. A pending journal survives closing the page or app.
pub struct RemoteMigration {
    scope: Option<Scope>,
    selected: BTreeSet<String>,
    team: Option<String>,
    reference: Option<String>,
    destination: Config,
    editor: Option<AnyView>,
    plan: Option<Plan>,
    journal: Option<FileJournal>,
    load_error: bool,
    pending: bool,
    attempted: bool,
    primary_focus: FocusHandle,
    dialog_focus: FocusHandle,
    focus_on_open: bool,
    error: Option<SharedString>,
}

impl EventEmitter<CloseMigration> for RemoteMigration {}

impl RemoteMigration {
    /// `team` preselects that team's exact keys; `None` permits any selection.
    pub fn new(team: Option<String>, cx: &mut Context<Self>) -> Self {
        let snapshot = Agents2::global(cx)
            .ok_or_else(|| "Agents is not running.".to_string())
            .and_then(|service| migration::snapshot(service.read(cx), cx));
        let mut this = Self {
            scope: None,
            selected: BTreeSet::new(),
            team,
            reference: None,
            destination: Config::default(),
            editor: None,
            plan: None,
            journal: None,
            load_error: false,
            pending: false,
            attempted: false,
            primary_focus: cx.focus_handle(),
            dialog_focus: cx.focus_handle().tab_stop(false),
            focus_on_open: true,
            error: None,
        };
        match snapshot {
            Ok(snapshot) => {
                this.destination = migration::defaults(&snapshot.scope);
                if let Some(team) = this.team.as_deref() {
                    match snapshot.roster(team) {
                        Ok(team) => this.selected = team.members,
                        Err(error) => this.error = Some(error.into()),
                    }
                }
                let journal = FileJournal::new(crate::community::data_dir(), &snapshot.scope);
                match journal.load(&snapshot.scope) {
                    Ok(Some(plan)) => {
                        this.attempted = true;
                        this.selected = plan
                            .items
                            .iter()
                            .map(|item| item.before.pubkey.clone())
                            .collect();
                        this.plan = Some(plan);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        this.error = Some(error.into());
                        this.load_error = true;
                    }
                }
                this.scope = Some(snapshot.scope);
                this.journal = Some(journal);
            }
            Err(error) => this.error = Some(error.into()),
        }
        if let Some(service) = Agents2::global(cx) {
            cx.observe(&service, |_, _, cx| cx.notify()).detach();
        }
        cx.observe_global::<crate::community::AppState>(|this, cx| {
            // Never carry an enrollment editor into another account/community.
            if migration::current_scope(cx).ok().as_ref() != this.scope.as_ref() {
                this.editor = None;
            }
            cx.notify();
        })
        .detach();
        this
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if !self.pending {
            cx.emit(CloseMigration);
        }
    }

    fn review(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let result = (|| {
            let service = Agents2::global(cx).ok_or("Agents is not running.")?;
            service.update(cx, |service, cx| {
                let snapshot = migration::snapshot(service, cx)?;
                if self.scope.as_ref() != Some(&snapshot.scope) {
                    return Err(
                        "Return to the account and community where you opened this dialog.".into(),
                    );
                }
                Plan::review(
                    &snapshot,
                    &self.selected,
                    self.team.as_deref(),
                    self.reference.as_deref(),
                    &self.destination,
                    |key| service.preview_retype(key, TARGET, cx),
                )
            })
        })();
        match result {
            Ok(plan) => {
                self.plan = Some(plan);
                self.editor = None;
            }
            Err(error) => self.error = Some(error.into()),
        }
        cx.notify();
    }

    fn choose_reference(&mut self, reference: Option<String>, cx: &mut Context<Self>) {
        if self.pending || self.plan.is_some() {
            return;
        }
        if migration::current_scope(cx).ok().as_ref() != self.scope.as_ref() {
            return;
        }
        let Some(service) = Agents2::global(cx) else {
            return;
        };
        self.destination = match &reference {
            Some(key) => match service.read(cx).find(key) {
                Some(agent) => Config::read(&agent.config),
                None => {
                    self.error = Some("The reference agent is no longer available.".into());
                    return;
                }
            },
            None => match &self.scope {
                Some(scope) => migration::defaults(scope),
                None => return,
            },
        };
        self.destination.env.clear();
        self.destination.instructions.clear();
        self.reference = reference;
        self.editor = None;
        self.error = None;
        cx.notify();
    }

    fn configure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if migration::current_scope(cx).ok().as_ref() != self.scope.as_ref() {
            return;
        }
        let Some(service) = Agents2::global(cx) else {
            return;
        };
        let first = self
            .selected
            .iter()
            .find_map(|key| service.read(cx).find(key).cloned());
        let Some(mut agent) = first else {
            self.error =
                Some("Select at least one agent before configuring the destination.".into());
            cx.notify();
            return;
        };
        if type_of(TARGET, cx).is_none() {
            return;
        }
        agent.agent_type = TARGET.into();
        agent.config = self.destination.value();
        let weak = cx.entity().downgrade();
        let pending = Rc::new(move |config: serde_json::Value, cx: &mut App| {
            let saved = weak
                .update(cx, |this, cx| {
                    if migration::current_scope(cx).ok().as_ref() != this.scope.as_ref() {
                        return Err("Return to the original account and community first.".into());
                    }
                    let mut config = Config::read(&config);
                    config.env.clear();
                    config.instructions.clear();
                    config.validate()?;
                    this.destination = config;
                    this.reference = None;
                    this.editor = None;
                    cx.notify();
                    Ok::<(), String>(())
                })
                .map_err(|_| "The migration dialog closed.".to_string())
                .and_then(|saved| saved);
            Task::ready(saved)
        });
        self.editor = Some(crate::agent_plugins::remote_workspace::migration_settings(
            AgentView {
                agent,
                conversation: None,
                settings_host: Some(SettingsHost {
                    type_control: None,
                    pending: Some(pending),
                }),
            },
            window,
            cx,
        ));
        cx.notify();
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        if self.pending || self.load_error {
            return;
        }
        let (Some(mut plan), Some(journal), Some(service)) =
            (self.plan.clone(), self.journal.clone(), Agents2::global(cx))
        else {
            return;
        };
        if plan.state != State::Active {
            return;
        }
        self.pending = true;
        self.attempted = true;
        self.error = None;
        cx.spawn(async move |this, cx| {
            let result = async {
                loop {
                    // The stale-state gate and retype admission share one GUI
                    // update. retype sets transitioning before it awaits, and
                    // the existing save path refuses config edits during it.
                    let step = service.update(cx, |service, cx| {
                        let snapshot = migration::snapshot(service, cx)?;
                        migration::checked_step(&mut plan, &snapshot, &journal, |item, operation| {
                            if operation == migration::Change::FinishConfig {
                                let state = service.run_state(&item.before.pubkey);
                                if state.working_since.is_some() || !state.held {
                                    return Task::ready(Err("Wait until this app holds the idle agent before resuming its configuration.".into()));
                                }
                                service.save(&item.before.pubkey, crate::agents2::AgentChange {
                                    config: Some(item.after.clone()), ..Default::default()
                                }, cx)
                            } else {
                                let task = service.retype(&item.before.pubkey, TARGET, Some(item.after.clone()), cx);
                                cx.spawn(async move |_, _| task.await.map(drop))
                            }
                        })
                    })?;
                    match step {
                        Step::Complete => break,
                        Step::Reconciled => {}
                        Step::Started(index, task) => {
                            task.await?;
                            plan.completed.insert(index);
                        }
                    }
                    this.update(cx, |this, cx| {
                        this.plan = Some(plan.clone()); cx.notify();
                    }).map_err(|_| "The migration dialog closed; reopen it to resume.".to_string())?;
                }
                Ok::<(), String>(())
            }.await;
            let _ = this.update(cx, |this, cx| {
                this.pending = false;
                this.plan = Some(plan);
                this.error = result.err().map(|error| format!("{error} Remaining agents were not changed. Resume when ready, or cancel the remaining migration.").into());
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn new_selection(&mut self, cx: &mut Context<Self>) {
        if self.pending || self.load_error {
            return;
        }
        if migration::current_scope(cx).ok().as_ref() != self.scope.as_ref() {
            self.error = Some("Return to the original account and community first.".into());
            cx.notify();
            return;
        }
        if let (Some(plan), Some(journal)) = (&mut self.plan, &self.journal)
            && plan.state == State::Active
        {
            plan.state = State::Cancelled;
            if let Err(error) = journal.save(plan) {
                plan.state = State::Active;
                self.error = Some(error.into());
                cx.notify();
                return;
            }
        }
        self.plan = None;
        self.attempted = false;
        self.focus_on_open = true;
        self.error = None;
        cx.notify();
    }

    fn archive_unreadable(&mut self, cx: &mut Context<Self>) {
        if !self.load_error
            || self.pending
            || migration::current_scope(cx).ok().as_ref() != self.scope.as_ref()
        {
            return;
        }
        let Some(journal) = &self.journal else {
            return;
        };
        match journal.archive() {
            Ok(()) => {
                self.load_error = false;
                self.error = None;
                self.focus_on_open = true;
            }
            Err(error) => self.error = Some(error.into()),
        }
        cx.notify();
    }
}

impl Render for RemoteMigration {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_on_open {
            self.focus_on_open = false;
            let focus = if self.plan.is_some() {
                self.primary_focus.clone()
            } else {
                self.dialog_focus.clone()
            };
            window.on_next_frame(move |window, cx| focus.focus(window, cx));
        }
        let c = *cx.colors();
        let snapshot =
            Agents2::global(cx).and_then(|service| migration::snapshot(service.read(cx), cx).ok());
        let same_scope = snapshot
            .as_ref()
            .is_some_and(|snapshot| Some(&snapshot.scope) == self.scope.as_ref());
        let key_labels = snapshot
            .as_ref()
            .map(|snapshot| {
                crate::ui::keys::public_key_labels(
                    snapshot.agents.iter().map(|agent| &agent.pubkey),
                )
            })
            .unwrap_or_default();
        let mut body = div()
            .id("remote-migration-body")
            .flex()
            .flex_col()
            .gap(rems(0.75))
            .max_h(rems(34.))
            .overflow_y_scroll();
        if !same_scope {
            body =
                body.child(div().text_color(c.text_warning).child(
                    "Return to the original account and community to continue this migration.",
                ));
        } else if let Some(plan) = &self.plan {
            let finished = plan.state != State::Active;
            body = body.child(div().text_label().child(format!(
                "{} of {} moved",
                plan.completed.len(),
                plan.items.len()
            )));
            for (index, item) in plan.items.iter().enumerate() {
                let config = Config::read(&item.after);
                let completed = plan.completed.contains(&index);
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rems(0.25))
                        .child(div().text_label_sm().child(format!(
                            "{} · {} · {}{}",
                            item.before.name,
                            key_labels.get(&item.before.pubkey).unwrap_or(&item.before.pubkey),
                            config.runtime,
                            if completed { " · moved" } else { "" }
                        )))
                        .child(
                            div()
                                .text_body_sm()
                                .text_color(c.text_subtle)
                                .child(format!(
                                    "Model: {}. Instructions move with this agent.",
                                    if config.model.is_empty() {
                                        "harness default"
                                    } else {
                                        &config.model
                                    }
                                )),
                        )
                        .children(item.warnings.iter().map(|warning| {
                            div()
                                .text_body_sm()
                                .text_color(c.text_warning)
                                .child(warning.clone())
                        }))
                        .children(
                            type_of(&item.before.kind, cx)
                                .and_then(|kind| kind.kind.migration_warning())
                                .map(|warning| {
                                    div()
                                        .text_body_sm()
                                        .text_color(c.text_warning)
                                        .child(warning)
                                }),
                        ),
                );
            }
            if !plan.already_remote.is_empty() {
                body = body.child(div().text_body_sm().child(format!(
                    "Already remote: {}",
                    plan.already_remote.join(", ")
                )));
            }
            body = body.child(div().text_body_sm().text_color(c.text_warning).child(
                "Keys, names, pictures, memberships, and attention stay with each identity. Local files and session history stay on this computer. Future mentions can start remote sessions. Saved watches and timers do not launch Remote workspace."));
            body = body.child(
                div()
                    .flex()
                    .gap(rems(0.5))
                    .when(!finished, |body| {
                        body.child(
                            Button::new(
                                "remote-migration-apply",
                                if self.attempted {
                                    "Resume migration"
                                } else {
                                    "Move selected agents"
                                },
                            )
                            .prominent()
                            .track_focus(&self.primary_focus)
                            .loading(self.pending)
                            .disabled(self.pending || self.load_error)
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
                        )
                    })
                    .child(
                        Button::new(
                            "remote-migration-review-again",
                            if finished {
                                "New selection"
                            } else {
                                "Cancel remaining and review again"
                            },
                        )
                        .disabled(self.pending || self.load_error)
                        .when(finished, |button| button.track_focus(&self.primary_focus))
                        .on_click(cx.listener(|this, _, _, cx| this.new_selection(cx))),
                    ),
            );
        } else if let Some(snapshot) = snapshot {
            body = body.child(div().text_body_sm().child("Select a saved team or choose individual existing agents. No new identities are created."));
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(rems(0.5))
                    .children(snapshot.teams.iter().enumerate().map(|(index, team)| {
                        let id = team.id.clone();
                        let members: BTreeSet<_> = team.members.iter().cloned().collect();
                        Button::new(("remote-migration-team", index), team.name.clone())
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.team = Some(id.clone());
                                this.selected = members.clone();
                                this.editor = None;
                                this.error = None;
                                cx.notify();
                            }))
                    }))
                    .child(
                        Button::new("remote-migration-all", "Select all agents")
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(service) = Agents2::global(cx) {
                                    this.selected = service
                                        .read(cx)
                                        .agents()
                                        .iter()
                                        .map(|agent| agent.pubkey.clone())
                                        .collect();
                                }
                                this.team = None;
                                this.editor = None;
                                cx.notify();
                            })),
                    ),
            );
            for (index, agent) in snapshot.agents.iter().enumerate() {
                let key = agent.pubkey.clone();
                let weak = cx.entity().downgrade();
                body = body.child(
                    Checkbox::new(
                        ("remote-migration-agent", index),
                        self.selected.contains(&key),
                    )
                    .label(format!(
                        "{} · {}{}",
                        agent.name,
                        key_labels.get(&key).unwrap_or(&key),
                        if agent.agent_type == TARGET {
                            " (already remote)"
                        } else {
                            ""
                        }
                    ))
                    .on_change(move |checked, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            if checked {
                                this.selected.insert(key.clone());
                            } else {
                                this.selected.remove(&key);
                            }
                            this.team = None;
                            this.editor = None;
                            cx.notify();
                        });
                    }),
                );
            }
            body = body
                .child(div().text_label_sm().child("Remote configuration"))
                .child(
                    Button::new("remote-migration-default", "New remote configuration")
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| this.choose_reference(None, cx))),
                );
            for (index, agent) in snapshot
                .agents
                .iter()
                .filter(|agent| agent.agent_type == TARGET)
                .enumerate()
            {
                let key = agent.pubkey.clone();
                body = body.child(
                    Button::new(
                        ("remote-migration-reference", index),
                        format!(
                            "Use {}'s configuration ({})",
                            agent.name,
                            key_labels.get(&key).unwrap_or(&key)
                        ),
                    )
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_reference(Some(key.clone()), cx)
                    })),
                );
            }
            body = body.child(div().text_body_sm().text_color(c.text_subtle).child(format!(
                "{} selected (maximum {}). Configuration: {}. Each Claude/Codex agent keeps its harness and model options. Credentials come from your enrolled workspace profile, not local logins or copied agent environments.",
                self.selected.len(), migration::LIMIT, self.reference.as_ref().and_then(|key| snapshot.agents.iter().find(|agent| &agent.pubkey == key)).map_or("new configuration", |agent| agent.name.as_str()))));
            body = body
                .child(
                    Button::new("remote-migration-configure", "Configure remote workspace")
                        .disabled(self.selected.is_empty() || self.load_error)
                        .on_click(cx.listener(|this, _, window, cx| this.configure(window, cx))),
                )
                .children(self.editor.clone());
            body = body.child(
                Button::new("remote-migration-preview", "Review migration")
                    .prominent()
                    .track_focus(&self.primary_focus)
                    .disabled(
                        self.selected.is_empty()
                            || self.selected.len() > migration::LIMIT
                            || self.editor.is_some()
                            || self.load_error,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.review(cx))),
            );
        }
        if same_scope && self.load_error {
            body = body.child(
                Button::new(
                    "remote-migration-archive",
                    "Archive unreadable record and start a fresh review",
                )
                .on_click(cx.listener(|this, _, _, cx| this.archive_unreadable(cx))),
            );
        }
        let dialog = Dialog::new("remote-migration-dialog", "Move agents to Remote workspace")
            .size(DialogSize::Wide)
            .on_close(cx.listener(|this, _, _, cx| this.close(cx)))
            .child(body)
            .children(
                self.error
                    .clone()
                    .filter(|_| same_scope)
                    .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            )
            .footer(
                Button::new("remote-migration-close", "Close")
                    .surface(Surface::Floating)
                    .disabled(self.pending)
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            );
        overlay::modal(
            window,
            div()
                .id("remote-migration-focus-scope")
                .key_context(buzz_ui::DIALOG_CONTEXT)
                .on_action(cx.listener(|this, _: &buzz_ui::Cancel, _, cx| this.close(cx)))
                .role(Role::Dialog)
                .aria_label("Move agents to Remote workspace")
                .track_focus(&self.dialog_focus)
                .child(dialog)
                .focus_trap("remote-migration-focus-trap", &self.dialog_focus),
            None,
        )
    }
}
