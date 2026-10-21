//! The sidebar's dialogs: "Create new section" (`CreateSidebarSection.tsx`),
//! "Remove <section>?" (`SidebarSection.tsx`) and the lifecycle
//! confirmation of a row action (`ChannelLifecycleDialog.tsx`).

use super::{ChannelSidebar, SidebarProps};
use crate::ui::channel_settings::{LifecycleDialog, LifecycleOutcome, lifecycle_modal};
use crate::ui::modal::error;
use crate::ui::overlay::{self, Dismiss};
use buzz_relay_client::lifecycle::Action;
use buzz_relay_client::sidebar::NewSection;
use buzz_relay_client::sidebar_store::PreferencesStatus;
use buzz_relay_client::{ChannelSummary, SessionSnapshot};
use buzz_ui::{ActiveTheme, Button, Dialog as DialogBox, Surface, TextField, TypeStyled};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// Longest section name (`maxLength`).
const NAME_LIMIT: usize = 256;

pub(super) enum Dialog {
    CreateSection {
        channel: Box<ChannelSummary>,
        input: Entity<InputState>,
        /// Chosen on the first submit and kept for retries.
        id: Option<String>,
        /// A submit found preferences unwritable.
        blocked: bool,
        _subscription: Subscription,
    },
    RemoveSection {
        id: String,
        title: String,
        count: usize,
        busy: bool,
        error: Option<SharedString>,
    },
    /// The confirmation shared with the header and settings panel, which
    /// keeps its action disabled after an unconfirmed result.
    Lifecycle {
        dialog: Entity<LifecycleDialog>,
        _subscription: Subscription,
    },
}

impl Dialog {
    pub(super) fn create_section(
        channel: ChannelSummary,
        window: &mut Window,
        cx: &mut Context<ChannelSidebar>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Section name"));
        input.update(cx, |state, cx| state.focus(window, cx));
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::PressEnter { .. } => this.submit_section(cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            );
        Self::CreateSection {
            channel: Box::new(channel),
            input,
            id: None,
            blocked: false,
            _subscription: subscription,
        }
    }
}

impl Dialog {
    pub(super) fn lifecycle(
        channel: ChannelSummary,
        action: Action,
        window: &mut Window,
        cx: &mut Context<ChannelSidebar>,
    ) -> Self {
        let community = cx.entity().read(cx).community.clone();
        let dialog = cx.new(|cx| LifecycleDialog::new(community, &channel, action, window, cx));
        let id = channel.id.clone();
        let subscription = cx.subscribe_in(
            &dialog,
            window,
            move |this: &mut ChannelSidebar, _, outcome: &LifecycleOutcome, window, cx| {
                this.dialog = None;
                if let LifecycleOutcome::Completed(action) = outcome
                    && matches!(action, Action::Leave | Action::Hide | Action::Delete)
                    && let Some(removed) = this.props.as_ref().map(|p| p.on_channel_removed.clone())
                {
                    removed(&id, window, cx);
                }
                cx.notify();
            },
        );
        Self::Lifecycle {
            dialog,
            _subscription: subscription,
        }
    }
}

impl ChannelSidebar {
    fn close_dialog(&mut self, cx: &mut Context<Self>) {
        self.dialog = None;
        cx.notify();
    }

    /// Moves the channel into a new section, if preferences can be saved.
    fn submit_section(&mut self, cx: &mut Context<Self>) {
        let writable = self.community.read(cx).snapshot.sidebar.writable;
        let Some(Dialog::CreateSection {
            channel,
            input,
            id,
            blocked,
            ..
        }) = &mut self.dialog
        else {
            return;
        };
        let name: String = input
            .read(cx)
            .value()
            .trim()
            .chars()
            .take(NAME_LIMIT)
            .collect();
        if name.is_empty() {
            return;
        }
        let id = id
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        if !writable {
            *blocked = true;
            cx.notify();
            return;
        }
        let channel = channel.id.clone();
        self.dialog = None;
        self.create_section(&channel, NewSection { id, name }, cx);
    }

    fn remove_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::RemoveSection {
            id, busy, error, ..
        }) = &mut self.dialog
        else {
            return;
        };
        if *busy {
            return;
        }
        *busy = true;
        *error = None;
        let id = id.clone();
        let session = self.session(cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = session.sidebar_remove_section(&id).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.dialog = None;
                        this.toggle(&format!("group:{id}"), true, cx);
                    }
                    Err(message) => {
                        if let Some(Dialog::RemoveSection { busy, error, .. }) = &mut this.dialog {
                            *busy = false;
                            *error = Some(message.into());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn dialog_overlay(
        &mut self,
        snapshot: &SessionSnapshot,
        _: &SidebarProps,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let c = *cx.colors();
        let dialog = self.dialog.as_ref()?;
        let weak = cx.entity().downgrade();
        let close: Dismiss = Rc::new(move |_, cx| {
            let _ = weak.update(cx, |this, cx| this.close_dialog(cx));
        });
        let (content, dismiss): (AnyElement, Option<Dismiss>) = match dialog {
            Dialog::CreateSection {
                channel,
                input,
                blocked,
                ..
            } => {
                let empty = input.read(cx).value().trim().is_empty();
                let writable = snapshot.sidebar.writable;
                let refreshing = snapshot.sidebar.status == PreferencesStatus::Loading;
                let dialog = DialogBox::new("create-section", "Create new section")
                    .description(format!("Move {} into a new section.", channel.name))
                    .on_close(cx.listener(|this, _, _, cx| this.close_dialog(cx)))
                    .child(TextField::new(input).surface(Surface::Floating))
                    .when(*blocked && !writable, |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(rems(0.5))
                                .child(error(
                                    "Saved preferences are unavailable. Retry preferences to \
                                     keep creating this section.",
                                    cx,
                                ))
                                .child(
                                    Button::new("retry-section-preferences", "Retry preferences")
                                        .surface(Surface::Floating)
                                        .disabled(refreshing)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.session(cx).refresh_sidebar()
                                        })),
                                ),
                        )
                    })
                    .footer(
                        Button::new("create-section-cancel", "Cancel")
                            .surface(Surface::Floating)
                            .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                    )
                    .footer(
                        Button::new("create-section-submit", "Create and move")
                            .prominent()
                            .disabled(empty)
                            .on_click(cx.listener(|this, _, _, cx| this.submit_section(cx))),
                    );
                (dialog.into_any_element(), Some(close))
            }
            Dialog::RemoveSection {
                title,
                count,
                busy,
                error: message,
                ..
            } => {
                let busy = *busy;
                let channels = if *count == 1 { "channel" } else { "channels" };
                let dialog = DialogBox::new("remove-section", format!("Remove {title}?"))
                    .description(format!(
                        "{count} {channels} will move back to Channels. This does not delete \
                         any channels."
                    ))
                    .on_close(cx.listener(move |this, _, _, cx| {
                        if !busy {
                            this.close_dialog(cx)
                        }
                    }))
                    .children(message.clone().map(|m| error(m, cx)))
                    .when(busy, |this| {
                        this.child(
                            div()
                                .text_body_sm()
                                .text_color(c.text_subtle)
                                .child("Removing section and waiting for confirmation…"),
                        )
                    })
                    .footer(
                        Button::new("remove-section-cancel", "Cancel")
                            .surface(Surface::Floating)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                    )
                    .footer(
                        Button::new("remove-section-confirm", "Remove section")
                            .destructive()
                            .loading(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.remove_section(window, cx)),
                            ),
                    );
                (dialog.into_any_element(), (!busy).then_some(close))
            }
            Dialog::Lifecycle { dialog, .. } => return Some(lifecycle_modal(dialog, window)),
        };
        Some(overlay::modal(window, content, dismiss).into_any_element())
    }
}
