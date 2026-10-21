//! A new conversation in Me (`sessions/NewSessionComposer.tsx`,
//! `SessionPresentation.tsx` `NewSessionView`).
//!
//! The first send starts the conversation step by step: a private work
//! session, its place in Me (and group), the group's defaults, then the
//! message. Every step's outcome is saved on this device first
//! ([`pending`]), so a failure or restart resumes where it stopped and
//! never makes a second session.

use super::controls::{self, AssistantChosen, ChatControls, ProjectChosen};
use super::dialogs::{self, DialogHost};
use super::pending::{self, PendingStart};
use super::recipients;
use super::settings::{self, SettingsTarget};
use super::{MeModel, Selection};
use crate::community::Community;
use crate::ui::composer::{Composer, ComposerEvent, Submission};
use crate::ui::overlay::{self, Align, Dismiss};
use buzz_relay_client::session::{Expected, MeKitStatus, session_title};
use buzz_relay_client::{EventId, Mentions, SessionStatus};
use buzz_ui::{
    ActiveTheme, Button, IconButton, IconButtonSize, IconName, Menu, MenuItem, PanelHeader,
    TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

pub struct NewSession {
    community: Entity<Community>,
    model: Entity<MeModel>,
    /// `me` or `me:section:<id>`.
    key: String,
    /// The project the chat starts in: the opened group's, until another
    /// is chosen below the composer.
    section: Option<String>,
    /// The follow-up assistant chosen below the composer, or empty.
    agent: String,
    controls: Entity<ChatControls>,
    composer: Entity<Composer>,
    pending: Option<PendingStart>,
    /// Unlocked to change the text after a failure before the message
    /// was queued.
    editing: bool,
    busy: bool,
    error: Option<SharedString>,
    /// The step that failed for good, which "Edit and retry" discards.
    failed: Option<EventId>,
    menu_open: bool,
    dialog: Option<dialogs::Dialog>,
    _subscriptions: Vec<Subscription>,
}

impl NewSession {
    /// `focus`: the composer takes the focus.
    pub fn new(
        community: Entity<Community>,
        key: String,
        section: Option<String>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let model = MeModel::global(cx);
        let composer = cx.new(|cx| {
            Composer::new(
                community.clone(),
                key.clone(),
                None,
                "Message your agents".into(),
                window,
                cx,
            )
        });
        composer.update(cx, |composer, cx| {
            composer.without_attachments();
            composer.invite_agents();
            if focus {
                composer.focus(window, cx)
            }
        });
        let pending = pending::read(&community, &key, cx);
        // A sent invitation fixes the assistant; otherwise the saved choice.
        let agent = match &pending {
            Some(start) if start.invitation_id.is_some() => start.agent.clone().unwrap_or_default(),
            _ => saved(&community, &format!("{key}:new-agent"), cx)
                .or_else(|| pending.as_ref().and_then(|start| start.agent.clone()))
                .filter(|agent| agent.len() == 64 && agent.bytes().all(|b| b.is_ascii_hexdigit()))
                .unwrap_or_default(),
        };
        let section = saved(&community, &format!("{key}:new-project"), cx)
            .map(|project| (!project.is_empty()).then_some(project))
            .unwrap_or(section);
        let controls = cx.new(|cx| ChatControls {
            assistant: cx
                .new(|_| controls::AssistantControl::new(community.clone(), agent.clone())),
            project: cx.new(|cx| {
                controls::ProjectControl::draft(
                    community.clone(),
                    section.clone().unwrap_or_default(),
                    cx,
                )
            }),
        });
        composer.update(cx, |composer, cx| {
            composer.set_trailing(controls.clone().into(), cx)
        });
        let (assistant, project) = {
            let controls = controls.read(cx);
            (controls.assistant.clone(), controls.project.clone())
        };
        let subscriptions = vec![
            cx.subscribe(&assistant, |this, _, AssistantChosen(agent), cx| {
                this.agent = agent.clone();
                save_view(
                    &this.community,
                    &format!("{}:new-agent", this.key),
                    Some(agent),
                    cx,
                );
                cx.notify();
            }),
            cx.subscribe(&project, |this, _, ProjectChosen(project), cx| {
                let next = (!project.is_empty()).then(|| project.clone());
                if next == this.section {
                    return;
                }
                this.section = next;
                save_view(
                    &this.community,
                    &format!("{}:new-project", this.key),
                    Some(project),
                    cx,
                );
                // The old project's draft settings no longer apply.
                let _ = pending::set_draft_canvas(&this.community, &this.key, None, cx);
                cx.notify();
            }),
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event: &ComposerEvent, window, cx| {
                    if let ComposerEvent::Submit(submission) = event {
                        this.start(submission.clone(), window, cx)
                    }
                },
            ),
            cx.observe(&community, |_, _, cx| cx.notify()),
            cx.observe(&model, |_, _, cx| cx.notify()),
        ];
        let mut this = Self {
            community,
            model,
            key,
            section,
            agent,
            controls,
            composer,
            pending,
            editing: false,
            busy: false,
            error: None,
            failed: None,
            menu_open: false,
            dialog: None,
            _subscriptions: subscriptions,
        };
        this.check_failed(cx);
        this
    }

    /// Starting needs a connected session, channel creation and Me
    /// placement; with a queued message only the session.
    fn available(&self, cx: &App) -> bool {
        let community = self.community.read(cx);
        let connected =
            community.snapshot.status == SessionStatus::Ready && !community.snapshot.cached;
        let queued = self
            .pending
            .as_ref()
            .is_some_and(|p| p.message_id.is_some());
        connected && (queued || self.model.read(cx).kit.status != MeKitStatus::Unavailable)
    }

    fn save(&mut self, start: PendingStart, cx: &mut Context<Self>) -> Result<(), String> {
        pending::save(&self.community, &self.key, &start, cx)?;
        self.pending = Some(start);
        Ok(())
    }

    /// Looks for a step that failed for good (`workSessions.failed`).
    fn check_failed(&mut self, cx: &mut Context<Self>) {
        let Some(start) = self.pending.clone() else {
            self.failed = None;
            return;
        };
        let session = self.community.read(cx).session.clone();
        cx.spawn(async move |this, cx| {
            let mut failed = None;
            for id in [&start.message_id, &start.invitation_id, &start.creation_id]
                .into_iter()
                .flatten()
            {
                if let Ok(id) = EventId::from_hex(id)
                    && session.work_session_failed(id).await
                {
                    failed = Some(id);
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.failed = failed;
                cx.notify();
            });
        })
        .detach();
    }

    fn start(&mut self, submission: Submission, window: &mut Window, cx: &mut Context<Self>) {
        let text = match &self.pending {
            Some(start) if start.message_id.is_some() => start.text.clone(),
            _ => submission.content.trim().to_owned(),
        };
        if self.busy || text.is_empty() || !self.available(cx) {
            self.composer.update(cx, |composer, cx| composer.failed(cx));
            return;
        }
        self.busy = true;
        self.editing = false;
        self.error = None;
        let mut current = match self.pending.clone() {
            Some(mut start) => {
                if start.message_id.is_none() {
                    start.text = text.clone();
                    start.recipients = submission.mentions.recipients.clone();
                    start.references = submission.mentions.references.clone();
                }
                start
            }
            None => PendingStart {
                id: uuid::Uuid::new_v4().to_string(),
                text: text.clone(),
                recipients: submission.mentions.recipients.clone(),
                references: submission.mentions.references.clone(),
                ..Default::default()
            },
        };
        let mentions = current.recipients.clone();
        // A plain first message goes to the chosen assistant; a mention
        // overrides it.
        if current.message_id.is_none() {
            current.agent =
                (mentions.is_empty() && !self.agent.is_empty()).then(|| self.agent.clone());
        }
        let mut recipients: Vec<String> = if mentions.is_empty() {
            current.agent.clone().into_iter().collect()
        } else {
            mentions.clone()
        };
        recipients.dedup();
        let community = self.community.clone();
        let session = community.read(cx).session.clone();
        let kit = session.me_kit().clone();
        let section = self.section.clone();
        let draft_canvas = pending::draft_canvas(&community, &self.key, cx);
        let rules = recipients::rules(&community, cx);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // Each step saves what it did before the next one runs.
            macro_rules! save {
                () => {
                    this.update(cx, |this, cx| this.save(current.clone(), cx))
                        .map_err(|_| "The view closed".to_owned())
                        .and_then(|saved| saved)
                };
            }
            let result: Result<(), String> = async {
                if let Some(setup) = &current.setup
                    && setup.section_id.is_some()
                    && setup.section_id != section
                {
                    return Err("The saved session belongs to a different section. Reopen its original section to retry.".into());
                }
                if current.setup.is_none() && current.creation_id.is_none() {
                    if let Some(section) = &section {
                        let mut setup =
                            settings::load_session_setup(&session, section, true).await?;
                        if let Some(canvas) = &draft_canvas {
                            setup.canvas = canvas.clone();
                        }
                        current.setup = Some(setup);
                    } else if let Some(canvas) = &draft_canvas {
                        current.setup = Some(pending::SessionSetup {
                            section_id: None,
                            canvas: canvas.clone(),
                            agents: Vec::new(),
                        });
                    }
                }
                save!()?;
                if current.message_id.is_none() {
                    let setup_agents = current
                        .setup
                        .as_ref()
                        .map(|s| s.agents.clone())
                        .unwrap_or_default();
                    if recipients
                        .iter()
                        .chain(&setup_agents)
                        .any(|key| !rules.selectable.contains(key))
                    {
                        return Err("Me can only use your available agents. Check mentions and section defaults.".into());
                    }
                }
                if current.creation_id.is_none() {
                    let section = current.setup.as_ref().and_then(|s| s.section_id.clone());
                    kit.admit(&current.id, section.as_deref()).await?;
                    let id = session
                        .work_session_create(&current.id, &session_title(&current.text), None)
                        .await?;
                    current.creation_id = Some(id.to_hex());
                    save!()?;
                }
                let creation = parse(current.creation_id.as_deref())?;
                session.work_session_delivered(creation).await?;
                session
                    .work_session_refresh(&current.id, Expected::default(), true)
                    .await?;
                if current.message_id.is_none() {
                    let section = current.setup.as_ref().and_then(|s| s.section_id.clone());
                    if !current.placement_done {
                        kit.set_placement(&current.id, true, section.as_deref()).await?;
                        current.placement_done = true;
                        save!()?;
                    } else {
                        kit.refresh().await;
                        if !kit.placed(&current.id) {
                            return Err("This conversation was moved out of Me. Review its placement before retrying.".into());
                        }
                    }
                }
                if let Some(setup) = current.setup.clone()
                    && !current.setup_done
                    && current.message_id.is_none()
                {
                    settings::apply_session_setup(&session, &current.id, &setup, true, &rules)
                        .await?;
                    current.setup_done = true;
                    save!()?;
                }
                if current.message_id.is_none()
                    && let Some(agent) = current.agent.clone()
                    && mentions.is_empty()
                {
                    if current.invitation_id.is_none() {
                        let id = session.work_session_invite(&current.id, &agent, &rules).await?;
                        current.invitation_id = Some(id.to_hex());
                        save!()?;
                    }
                    let invitation = parse(current.invitation_id.as_deref())?;
                    session.work_session_delivered(invitation).await?;
                    session
                        .work_session_refresh(&current.id, Expected::member(&agent), true)
                        .await?;
                }
                if current.message_id.is_none() {
                    if !mentions.is_empty() {
                        session.add_agents(&current.id, &recipients, &rules).await?;
                    }
                    if !kit.placed(&current.id) {
                        return Err(
                            "This conversation is no longer personal. Open it in Messages.".into(),
                        );
                    }
                    session
                        .send(
                            &current.id,
                            &current.text,
                            Mentions {
                                recipients: recipients.clone(),
                                references: current.references.clone(),
                                ..Default::default()
                            },
                            Vec::new(),
                            None,
                        )
                        .await?;
                    let id = session
                        .queued_message(&current.id, &current.text)
                        .await
                        .ok_or("The operation could not be confirmed.")?;
                    current.message_id = Some(id.to_hex());
                    save!()?;
                }
                let message = parse(current.message_id.as_deref())?;
                session.work_session_delivered(message).await
            }
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        pending::clear(&this.community, &this.key, cx);
                        let _ = pending::set_draft_canvas(&this.community, &this.key, None, cx);
                        let key = this.key.clone();
                        save_view(&this.community, &format!("{key}:new-agent"), None, cx);
                        save_view(&this.community, &format!("{key}:new-project"), None, cx);
                        controls::write_assistant(&this.community, &current.id, &this.agent, cx);
                        this.pending = None;
                        this.composer
                            .update(cx, |composer, cx| composer.sent(window, cx));
                        let id = current.id.clone();
                        let placed = this.model.read(cx).placed(&id);
                        if placed {
                            this.model.update(cx, |model, cx| {
                                model.select(Selection::Session(id), cx)
                            });
                        } else {
                            crate::ui::navigation::open_conversation(id, window, cx);
                        }
                    }
                    Err(error) => {
                        if this
                            .pending
                            .as_ref()
                            .is_none_or(|p| p.message_id.is_none())
                        {
                            this.editing = true;
                        }
                        this.error = Some(error.into());
                        this.composer.update(cx, |composer, cx| composer.failed(cx));
                        this.check_failed(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// "Edit and retry": discards the failed step so the draft can change.
    fn edit_failed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(start), Some(failed)) = (self.pending.clone(), self.failed) else {
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        let session = self.community.read(cx).session.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.work_session_discard(failed).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        let hex = failed.to_hex();
                        if start.creation_id.as_deref() == Some(hex.as_str()) {
                            pending::clear(&this.community, &this.key, cx);
                            this.pending = None;
                        } else {
                            let mut next = start.clone();
                            if next.invitation_id.as_deref() == Some(hex.as_str()) {
                                next.invitation_id = None;
                                next.agent = None;
                                // A rejected invitation's assistant is chosen again.
                                this.agent.clear();
                                let key = format!("{}:new-agent", this.key);
                                save_view(&this.community, &key, None, cx);
                                let assistant = this.controls.read(cx).assistant.clone();
                                assistant
                                    .update(cx, |control, cx| control.set_value(String::new(), cx));
                            } else {
                                next.message_id = None;
                            }
                            if let Err(error) = this.save(next, cx) {
                                this.error = Some(error.into());
                                cx.notify();
                                return;
                            }
                        }
                        this.failed = None;
                        this.editing = true;
                        this.error = None;
                    }
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// "Continue without section" after the group was removed.
    fn drop_section(&mut self, cx: &mut Context<Self>) {
        let Some(mut start) = self.pending.clone() else {
            return;
        };
        if let Some(setup) = &mut start.setup {
            setup.section_id = None;
        }
        match self.save(start, cx) {
            Ok(()) => {
                self.error = None;
                self.editing = true;
            }
            Err(error) => self.error = Some(error.into()),
        }
        cx.notify();
    }

    fn header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu_open = self.menu_open;
        let disabled = self.busy || self.pending.is_some();
        let menu = menu_open.then(|| {
            let weak = cx.entity().downgrade();
            let dismiss: Dismiss = Rc::new(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.menu_open = false;
                    cx.notify();
                });
            });
            let content = Menu::new().child(
                MenuItem::new("me-new-settings", "Chat settings…")
                    .icon(IconName::Gear)
                    .icon_size(16.)
                    .disabled(disabled)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.menu_open = false;
                        let target = SettingsTarget::Draft {
                            key: this.key.clone(),
                            section: this.section.clone(),
                        };
                        let dialog =
                            dialogs::Dialog::settings(this.community.clone(), target, window, cx);
                        this.dialog = Some(dialog);
                        cx.notify();
                    })),
            );
            overlay::dropdown(window, Align::End, content, dismiss)
        });
        PanelHeader::new()
            .joined(true)
            .label(Some(IconName::Hash), "New chat")
            .action(
                div()
                    .relative()
                    .child(
                        IconButton::new("me-new-actions", IconName::DotsThree)
                            .size(IconButtonSize::Sm)
                            .selected(menu_open)
                            .when(!menu_open, |b| b.tooltip("Session actions"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.menu_open = true;
                                cx.notify();
                            })),
                    )
                    .children(menu),
            )
    }
}

/// A new chat's saved string choice (`readView`).
fn saved(community: &Entity<Community>, key: &str, cx: &App) -> Option<String> {
    cx.global::<crate::community::AppState>()
        .store
        .view_state(&community.read(cx).partition(), key)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<String>(&json).ok())
}

/// Saves (or with `None`, clears) a new chat's choice.
fn save_view(community: &Entity<Community>, key: &str, value: Option<&str>, cx: &App) {
    let json = value.map(|value| serde_json::to_string(value).expect("strings serialize"));
    if let Err(error) = cx
        .global::<crate::community::AppState>()
        .store
        .set_view_state(&community.read(cx).partition(), key, json.as_deref())
    {
        tracing::warn!(%error, "could not save the new chat's choice");
    }
}

fn parse(id: Option<&str>) -> Result<EventId, String> {
    id.and_then(|id| EventId::from_hex(id).ok())
        .ok_or_else(|| "The operation could not be confirmed.".to_owned())
}

impl DialogHost for NewSession {
    fn dialog(&mut self) -> &mut Option<dialogs::Dialog> {
        &mut self.dialog
    }
}

impl Render for NewSession {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let available = self.available(cx);
        // A saved start is retried with the composer's text; only a running
        // start or a missing connection holds the send.
        let busy = self.busy;
        self.composer.update(cx, |composer, cx| {
            composer.set_can_send(available && !busy, cx)
        });
        let section_name = self.section.as_ref().map(|section| {
            self.model
                .read(cx)
                .me
                .data
                .as_ref()
                .and_then(|data| data.sections.iter().find(|s| s.id == *section))
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "this project".into())
        });
        // The assistant is fixed once its invitation or the message is
        // sent; the project once anything is.
        let (assistant, project) = {
            let controls = self.controls.read(cx);
            (controls.assistant.clone(), controls.project.clone())
        };
        let fixed = self
            .pending
            .as_ref()
            .is_some_and(|p| p.invitation_id.is_some() || p.message_id.is_some());
        let started = self.pending.is_some();
        assistant.update(cx, |control, cx| control.set_disabled(busy || fixed, cx));
        project.update(cx, |control, cx| control.set_disabled(busy || started, cx));
        let note = |text: &str| {
            div()
                .mt(rems(0.75))
                .mx(rems(1.))
                .text_caption()
                .text_color(c.text_subtle)
                .child(text.to_owned())
        };
        let removed = self.error.as_deref() == Some(buzz_relay_client::kit::me::REMOVED_SECTION)
            && self.pending.as_ref().is_some_and(|p| {
                p.setup.as_ref().is_some_and(|s| s.section_id.is_some())
                    && !p.setup_done
                    && p.message_id.is_none()
            });
        let prompt = div()
            .id("new-session-prompt")
            .flex()
            .flex_col()
            .w_full()
            .mt_auto()
            .when_some(section_name, |this, name| {
                this.child(note(&format!("New chat in {name}")).mb(rems(0.5)))
            })
            .child(self.composer.clone())
            .when(!available, |this| {
                this.child(note(
                    "Me preferences or channel creation are unavailable. Your draft is saved.",
                ))
            })
            .when(
                self.pending.as_ref().is_some_and(|p| p.agent.is_some()),
                |this| {
                    this.child(note(
                        "Recovering an earlier send with a saved agent recipient.",
                    ))
                },
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .mt(rems(0.75))
                        .mx(rems(1.))
                        .text_body_sm()
                        .text_color(c.text_danger)
                        .child(error),
                )
            })
            .when(removed, |this| {
                this.child(
                    div().mt(rems(0.5)).mx(rems(1.)).child(
                        Button::new("me-continue-without-section", "Continue without section")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.drop_section(cx))),
                    ),
                )
            })
            .when(self.error.is_some() && self.failed.is_some(), |this| {
                this.child(
                    div().mt(rems(0.5)).mx(rems(1.)).child(
                        Button::new("me-edit-retry", "Edit and retry")
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.edit_failed(window, cx)),
                            ),
                    ),
                )
            });
        let header = self.header(window, cx);
        div()
            .id("me-new-session")
            .size_full()
            .flex()
            .flex_col()
            .min_w_0()
            .child(header)
            .child(
                div()
                    .id("me-new-start")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .overflow_y_scroll()
                    .child(prompt),
            )
            .children(
                self.dialog
                    .as_ref()
                    .map(|dialog| dialog.overlay(window, cx)),
            )
            .text_color(c.text_standard)
    }
}
