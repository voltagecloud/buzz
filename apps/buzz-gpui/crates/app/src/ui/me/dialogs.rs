//! Me's dialogs, shown over the sidebar or the conversation that opened
//! them: "Create new project", "Rename session", "Project settings" and
//! "Share conversation". Each is its own view and reports back through
//! [`DialogEvent`].

use super::rename::RenameSession;
use super::settings::{SettingsTarget, WorkspaceSettings};
use super::share::SessionShare;
use crate::community::Community;
use crate::ui::overlay::{self, Dismiss};
use buzz_relay_client::ChannelSummary;
use buzz_ui::{ActiveTheme, Button, Dialog as DialogBox, Surface, TextField, TypeStyled};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

/// What a dialog asks of the view that opened it.
pub enum DialogEvent {
    Close,
    /// The conversation moved to Messages.
    Shared(String),
    /// "Create and move": a new group named this, the row moved in.
    CreateSection {
        channel: String,
        name: String,
    },
}

/// The view that hosts Me's dialogs.
pub trait DialogHost: 'static + Sized {
    fn dialog(&mut self) -> &mut Option<Dialog>;

    fn close_dialog(&mut self, cx: &mut Context<Self>) {
        *self.dialog() = None;
        cx.notify();
    }

    /// "Create and move"; false when groups cannot be saved now.
    fn create_section(&mut self, _channel: String, _name: String, _cx: &mut Context<Self>) -> bool {
        false
    }

    fn shared(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.close_dialog(cx);
        crate::ui::navigation::open_conversation(id.to_owned(), window, cx);
    }
}

pub struct Dialog {
    view: AnyView,
    /// A press on the scrim closes it (not while it is busy).
    dismissible: Rc<dyn Fn(&App) -> bool>,
    _subscription: Subscription,
}

impl Dialog {
    fn wrap<T: DialogHost, V: Render + EventEmitter<DialogEvent>>(
        view: Entity<V>,
        dismissible: impl Fn(&V) -> bool + 'static,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        let subscription =
            cx.subscribe_in(
                &view,
                window,
                |this: &mut T, _, event, window, cx| match event {
                    DialogEvent::Close => this.close_dialog(cx),
                    DialogEvent::Shared(id) => this.shared(id, window, cx),
                    DialogEvent::CreateSection { channel, name } => {
                        if this.create_section(channel.clone(), name.clone(), cx) {
                            this.close_dialog(cx);
                        } else if let Some(dialog) = this.dialog().as_ref()
                            && let Ok(view) = dialog.view.clone().downcast::<NewSectionDialog>()
                        {
                            view.update(cx, |view, cx| {
                                view.blocked = true;
                                cx.notify();
                            });
                        }
                    }
                },
            );
        let weak = view.downgrade();
        Self {
            view: view.into(),
            dismissible: Rc::new(move |cx| {
                weak.upgrade()
                    .is_some_and(|view| dismissible(view.read(cx)))
            }),
            _subscription: subscription,
        }
    }

    pub fn new_section<T: DialogHost>(
        channel: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        let view = cx.new(|cx| NewSectionDialog::new(channel, title, window, cx));
        Self::wrap(view, |_| true, window, cx)
    }

    pub fn rename<T: DialogHost>(
        community: Entity<Community>,
        id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        let view = cx.new(|cx| RenameSession::new(community, id, name, window, cx));
        Self::wrap(view, |view| !view.busy(), window, cx)
    }

    pub fn share<T: DialogHost>(
        community: Entity<Community>,
        channel: ChannelSummary,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        let view = cx.new(|cx| SessionShare::new(community, channel, window, cx));
        Self::wrap(view, |view| !view.busy(), window, cx)
    }

    pub fn session_settings<T: DialogHost>(
        community: Entity<Community>,
        id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        Self::settings(community, SettingsTarget::Session { id, name }, window, cx)
    }

    pub fn section_settings<T: DialogHost>(
        community: Entity<Community>,
        id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        Self::settings(community, SettingsTarget::Section { id, name }, window, cx)
    }

    pub fn settings<T: DialogHost>(
        community: Entity<Community>,
        target: SettingsTarget,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        let view = cx.new(|cx| WorkspaceSettings::new(community, target, window, cx));
        Self::wrap(view, WorkspaceSettings::dismissible, window, cx)
    }

    /// The dialog over its scrim.
    pub fn overlay<T: DialogHost>(&self, window: &Window, cx: &mut Context<T>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let dismissible = self.dismissible.clone();
        let close: Dismiss = Rc::new(move |_, cx| {
            if dismissible(cx) {
                let _ = weak.update(cx, |this, cx| this.close_dialog(cx));
            }
        });
        overlay::modal(window, self.view.clone(), Some(close)).into_any_element()
    }
}

/// "Create new project" (`NewSection` in `SessionSections.tsx`).
pub struct NewSectionDialog {
    channel: String,
    title: String,
    input: Entity<InputState>,
    /// A submit found sections unavailable.
    pub blocked: bool,
    _subscription: Subscription,
}

/// `maxLength` of the name.
const NAME_LIMIT: usize = 80;

impl EventEmitter<DialogEvent> for NewSectionDialog {}

impl NewSectionDialog {
    fn new(channel: String, title: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Project name"));
        input.update(cx, |state, cx| state.focus(window, cx));
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, _, cx| match event {
                    InputEvent::PressEnter { .. } => this.submit(cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            );
        Self {
            channel,
            title,
            input,
            blocked: false,
            _subscription: subscription,
        }
    }

    fn name(&self, cx: &App) -> String {
        self.input
            .read(cx)
            .value()
            .trim()
            .chars()
            .take(NAME_LIMIT)
            .collect()
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let name = self.name(cx);
        if name.is_empty() {
            return;
        }
        cx.emit(DialogEvent::CreateSection {
            channel: self.channel.clone(),
            name,
        });
    }
}

impl Render for NewSectionDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.name(cx).is_empty();
        DialogBox::new("me-new-section", "Create new project")
            .description(format!(
                "Move {} into a personal project that syncs across Buzz.",
                self.title
            ))
            .on_close(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Close)))
            .child(TextField::new(&self.input).surface(Surface::Floating))
            .when(self.blocked, |this| {
                this.child(
                    div()
                        .text_body_sm()
                        .text_color(cx.colors().text_danger)
                        .child("Projects are unavailable. Close this dialog and retry."),
                )
            })
            .footer(
                Button::new("me-new-section-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Close))),
            )
            .footer(
                Button::new("me-new-section-submit", "Create and move")
                    .prominent()
                    .disabled(empty)
                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
            )
    }
}
