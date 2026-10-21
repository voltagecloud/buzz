//! The private key widget (`PrivateKey.tsx`): a masked, read-only field
//! with Reveal/Hide and Copy. The key is cleared when the window loses
//! focus or the widget hides. Settings → Profile and the sign-out dialog
//! each have their own; the dialog learns when the key was revealed or
//! copied.

use crate::community::{AppState, IdentityState};
use buzz_ui::{ActiveTheme, Button, ButtonShape, Surface, TextField, TypeStyled};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use zeroize::Zeroizing;

/// What the field shows while hidden.
const MASK: &str = "••••••••••••••••";

/// The key was revealed or copied (`onInteraction`).
pub struct KeyInteraction;

pub struct PrivateKey {
    field: Entity<InputState>,
    /// The revealed key; `None` while hidden.
    secret: Option<Zeroizing<String>>,
    /// The last reveal or copy outcome; `true` when it failed.
    message: Option<(SharedString, bool)>,
    /// Settings buttons use 12px corners; the dialog's are pills.
    control_shape: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<KeyInteraction> for PrivateKey {}

impl PrivateKey {
    pub fn new(control_shape: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = cx.new(|cx| InputState::new(window, cx).default_value(MASK));
        let _subscriptions = vec![
            // Read-only: the key can be selected and copied, not edited.
            cx.subscribe_in(&field, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.show(window, cx);
                }
            }),
            // Leaving the window hides the key.
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.hide(cx);
                }
            }),
        ];
        Self {
            field,
            secret: None,
            message: None,
            control_shape,
            _subscriptions,
        }
    }

    pub fn hide(&mut self, cx: &mut Context<Self>) {
        if self.secret.is_some() || self.message.is_some() {
            self.secret = None;
            self.message = None;
            cx.notify();
        }
    }

    /// Puts the field back to what it shows, undoing any typing.
    fn show(&self, window: &mut Window, cx: &mut App) {
        let text = self.secret.as_deref().map_or(MASK, String::as_str);
        if self.field.read(cx).value() != text {
            let text = text.to_string();
            self.field
                .update(cx, |state, cx| state.set_value(text, window, cx));
        }
    }

    fn key(cx: &App) -> Option<Zeroizing<String>> {
        match &cx.global::<AppState>().identity {
            IdentityState::Ready(identity) => Some(identity.nsec()),
            _ => None,
        }
    }

    fn reveal(&mut self, cx: &mut Context<Self>) {
        self.message = None;
        match Self::key(cx) {
            Some(key) => {
                self.secret = Some(key);
                cx.emit(KeyInteraction);
            }
            None => {
                self.message = Some((
                    "Couldn’t reveal your private key. Retry when secure storage is available."
                        .into(),
                    true,
                ))
            }
        }
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.secret.clone().or_else(|| Self::key(cx)) else {
            self.message = Some((
                "Couldn’t copy your private key. Reveal it to copy manually, or retry.".into(),
                true,
            ));
            cx.notify();
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(key.to_string()));
        self.message = Some((
            "Private key copied. Keep it somewhere safe; your clipboard now contains it.".into(),
            false,
        ));
        cx.emit(KeyInteraction);
        cx.notify();
    }
}

impl Render for PrivateKey {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        self.show(window, cx);
        let revealed = self.secret.is_some();
        let shape = if self.control_shape {
            ButtonShape::Control
        } else {
            ButtonShape::Capsule
        };
        // `<div className="mt-6">`.
        div()
            .mt(rems(1.5))
            .flex()
            .flex_col()
            .child(
                div()
                    .mb(rems(0.5))
                    .text_label_sm()
                    .child("Private key (nsec)"),
            )
            .child(
                div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child("Anyone with this key can act as you. Never share it."),
            )
            .child(
                div()
                    .mt(rems(0.75))
                    .child(TextField::new(&self.field).surface(Surface::Floating)),
            )
            .child(
                div()
                    .mt(rems(0.75))
                    .flex()
                    .flex_wrap()
                    .gap(rems(0.75))
                    .child(
                        Button::new(
                            "reveal-nsec",
                            if revealed {
                                "Hide private key"
                            } else {
                                "Reveal private key"
                            },
                        )
                        .shape(shape)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.secret.is_some() {
                                this.hide(cx);
                            } else {
                                this.reveal(cx);
                            }
                        })),
                    )
                    .child(
                        Button::new("copy-nsec", "Copy private key")
                            .shape(shape)
                            .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                    ),
            )
            .children(self.message.clone().map(|(text, failed)| {
                div()
                    .mt(rems(0.75))
                    .text_body_sm()
                    .text_color(if failed { c.text_danger } else { c.text_subtle })
                    .child(text)
            }))
    }
}
