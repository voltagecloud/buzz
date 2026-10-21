//! "Rename session" (`sessions/RenameSession.tsx`). The session's channel
//! details own permission, conflict checks and delivery; a save that may
//! have reached the relay is checked, never sent again.

use super::dialogs::DialogEvent;
use crate::community::Community;
use buzz_relay_client::lifecycle::{AttemptStatus, ChannelDetails, canonical_name};
use buzz_ui::{ActiveTheme, Button, Dialog, Surface, TextField, TypeStyled};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

pub struct RenameSession {
    community: Entity<Community>,
    id: String,
    input: Entity<InputState>,
    base: Option<ChannelDetails>,
    busy: bool,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl EventEmitter<DialogEvent> for RenameSession {}

enum Run {
    Save,
    Reload,
    Check,
}

impl RenameSession {
    pub fn new(
        community: Entity<Community>,
        id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let attempt = community.read(cx).session.details_attempt(&id);
        let value = attempt.map(|a| a.draft.name).unwrap_or(name);
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value(value, window, cx);
            state
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    if this.attempt(cx).is_none() {
                        this.run(Run::Save, window, cx)
                    }
                }
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        );
        let mut this = Self {
            community,
            id,
            input,
            base: None,
            busy: false,
            error: None,
            _subscription: subscription,
        };
        this.load(window, cx);
        this
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    fn attempt(&self, cx: &App) -> Option<buzz_relay_client::lifecycle::DetailsAttempt> {
        self.community.read(cx).session.details_attempt(&self.id)
    }

    fn name(&self, cx: &App) -> String {
        canonical_name(&self.input.read(cx).value())
    }

    fn valid(&self, cx: &App) -> bool {
        let name = self.name(cx);
        !name.is_empty() && name.chars().count() <= 120
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.community.read(cx).session.clone();
        let id = self.id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.channel_details(&id).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(base) => this.base = Some(base),
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn run(&mut self, run: Run, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let session = self.community.read(cx).session.clone();
        let id = self.id.clone();
        let save = match run {
            Run::Save => {
                let Some(base) = self.base.clone().filter(|b| b.can_edit) else {
                    return;
                };
                if !self.valid(cx) {
                    return;
                }
                let mut draft = base.draft.clone();
                draft.name = self.name(cx);
                Some((base.version, draft))
            }
            _ => None,
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = match run {
                Run::Reload => session.channel_details(&id).await.map(Some),
                Run::Check => session.check_channel_details(&id).await.map(|()| None),
                Run::Save => {
                    let (version, draft) = save.expect("a save has its draft");
                    session
                        .save_channel_details(&id, version, draft)
                        .await
                        .map(|()| None)
                }
            };
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(Some(base)) => this.base = Some(base),
                    Ok(None) => cx.emit(DialogEvent::Close),
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for RenameSession {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let attempt = self.attempt(cx);
        let unconfirmed = attempt
            .as_ref()
            .is_some_and(|a| a.status == AttemptStatus::Unconfirmed);
        let valid = self.valid(cx);
        let can_edit = self.base.as_ref().is_some_and(|b| b.can_edit);
        let busy = self.busy;
        let field = div()
            .flex()
            .flex_col()
            .gap(rems(0.5))
            .child(div().text_label().child("Session name"))
            .child(
                TextField::new(&self.input)
                    .surface(Surface::Floating)
                    .invalid(!valid)
                    .disabled(busy || attempt.is_some()),
            )
            .when(!valid, |this| {
                this.child(
                    div()
                        .text_body_sm()
                        .text_color(c.text_danger)
                        .child("Enter a name of 1–120 characters."),
                )
            });
        let mut dialog = Dialog::new("me-rename", "Rename session")
            .on_close(cx.listener(move |_, _, _, cx| {
                if !busy {
                    cx.emit(DialogEvent::Close)
                }
            }))
            .child(field)
            .when(self.base.as_ref().is_some_and(|b| !b.can_edit), |this| {
                this.child(
                    div()
                        .text_body_sm()
                        .text_color(c.text_subtle)
                        .child("You don’t have permission to rename this session."),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap(rems(0.5))
                        .child(div().text_body_sm().text_color(c.text_danger).child(error))
                        .when(attempt.is_none(), |this| {
                            this.child(
                                Button::new("me-rename-reload", "Reload session details")
                                    .surface(Surface::Floating)
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.run(Run::Reload, window, cx)
                                    })),
                            )
                        }),
                )
            })
            .footer(
                Button::new("me-rename-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .disabled(busy)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Close))),
            );
        dialog = if unconfirmed {
            dialog.footer(
                Button::new("me-rename-check", "Check saved name")
                    .surface(Surface::Floating)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.run(Run::Check, window, cx))),
            )
        } else {
            dialog.footer(
                Button::new("me-rename-save", "Save")
                    .prominent()
                    .disabled(busy || attempt.is_some() || !can_edit || !valid)
                    .on_click(cx.listener(|this, _, window, cx| this.run(Run::Save, window, cx))),
            )
        };
        dialog
    }
}
