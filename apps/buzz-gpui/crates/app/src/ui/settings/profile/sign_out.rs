//! The sign-out dialog (`SignOut.tsx`): the private key widget, then
//! "I have my key" once the key was revealed or copied, and the optional
//! erase with "Also remove my agents" and the typed phrase. Confirming
//! records the sign-out and restarts Buzz; the next launch removes the
//! key (`crate::sign_out`).

use super::private_key::{KeyInteraction, PrivateKey};
use crate::sign_out::{self, Choices};
use buzz_ui::{ActiveTheme, AlertDialog, Button, Checkbox, Surface, TextField, TypeStyled};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

pub const WIPE_PHRASE: &str = "erase all my data";

/// Cancel: close the dialog.
pub struct SignOutClosed;

/// Confirm only after the key was revealed or copied and the user says
/// they have it (`signOutReady`).
pub fn sign_out_ready(touched: bool, have_key: bool, wipe: bool, phrase: &str) -> bool {
    touched && have_key && (!wipe || phrase.trim() == WIPE_PHRASE)
}

/// A checkbox handler that updates the dialog.
fn on_check(
    cx: &Context<SignOutDialog>,
    change: impl Fn(&mut SignOutDialog, bool, &mut Window, &mut Context<SignOutDialog>) + 'static,
) -> impl Fn(bool, &mut Window, &mut App) + 'static {
    let dialog = cx.entity().downgrade();
    move |checked, window, cx| {
        let _ = dialog.update(cx, |dialog, cx| change(dialog, checked, window, cx));
    }
}

pub struct SignOutDialog {
    key: Entity<PrivateKey>,
    touched: bool,
    have_key: bool,
    wipe: bool,
    remove_agents: bool,
    phrase: Entity<InputState>,
    pending: bool,
    error: Option<SharedString>,
    /// Why this build can't erase; the native side enforces it too.
    wipe_refusal: Option<&'static str>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SignOutClosed> for SignOutDialog {}

impl SignOutDialog {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let key = cx.new(|cx| PrivateKey::new(false, window, cx));
        let phrase = cx.new(|cx| InputState::new(window, cx));
        let _subscriptions = vec![
            cx.subscribe(&key, |this, _, _: &KeyInteraction, cx| {
                this.touched = true;
                cx.notify();
            }),
            cx.subscribe(&phrase, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        Self {
            key,
            touched: false,
            have_key: false,
            wipe: false,
            remove_agents: false,
            phrase,
            pending: false,
            error: None,
            wipe_refusal: sign_out::wipe_refusal(),
            _task: None,
            _subscriptions,
        }
    }

    /// While signing out the dialog can't be dismissed.
    pub fn pending(&self) -> bool {
        self.pending
    }

    fn ready(&self, cx: &App) -> bool {
        sign_out_ready(
            self.touched,
            self.have_key,
            self.wipe,
            &self.phrase.read(cx).value(),
        )
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if !self.ready(cx) || self.pending {
            return;
        }
        self.pending = true;
        self.error = None;
        let choices = Choices {
            wipe: self.wipe,
            remove_agents: self.wipe && self.remove_agents,
        };
        let signing_out = sign_out::sign_out(choices, cx);
        self._task = Some(cx.spawn(async move |this, cx| {
            // Success restarts Buzz; only a refusal comes back.
            if let Err(error) = signing_out.await {
                let _ = this.update(cx, |this, cx| {
                    this.error = Some(error.into());
                    this.pending = false;
                    cx.notify();
                });
            }
        }));
        cx.notify();
    }

    fn set_wipe(&mut self, wipe: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.wipe = wipe;
        if !wipe {
            self.remove_agents = false;
            self.phrase
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
        cx.notify();
    }
}

impl Render for SignOutDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let pending = self.pending;
        let ready = self.ready(cx);
        let note = |text: String| div().text_body_sm().text_color(c.text_subtle).child(text);
        let erase_note = {
            let mut text = "Erasing clears this app’s data, local storage, caches and plugin \
                            storage on this device only. It can’t reach your clipboard, keys you \
                            exported, or relay data; your npub and its history stay on the relays."
                .to_string();
            if !self.remove_agents {
                text.push_str(
                    " Kept agents keep their names, types and keys; their settings on this \
                     device are erased.",
                );
            }
            text
        };
        let options = div()
            .mt(rems(1.5))
            .flex()
            .flex_col()
            .gap(rems(0.75))
            .child(
                Checkbox::new("sign-out-have-key", self.have_key)
                    .label("I have my key")
                    .disabled(!self.touched || pending)
                    .on_change(on_check(cx, |this, checked, _, cx| {
                        this.have_key = checked;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("sign-out-wipe", self.wipe)
                    .label("Also erase everything else Buzz stores on this device")
                    .disabled(pending || self.wipe_refusal.is_some())
                    .on_change(on_check(cx, |this, checked, window, cx| {
                        this.set_wipe(checked, window, cx)
                    })),
            )
            .children(self.wipe_refusal.map(|reason| note(format!("{reason}."))))
            .when(self.wipe, |this| {
                this.child(
                    Checkbox::new("sign-out-remove-agents", self.remove_agents)
                        .label("Also remove my agents")
                        .disabled(pending)
                        .on_change(on_check(cx, |this, checked, _, cx| {
                            this.remove_agents = checked;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rems(0.5))
                        .child(
                            div()
                                .text_label_sm()
                                .child(format!("Type “{WIPE_PHRASE}” to confirm")),
                        )
                        .child(
                            TextField::new(&self.phrase)
                                .surface(Surface::Floating)
                                .disabled(pending),
                        ),
                )
                .child(note(erase_note))
            })
            .when(!self.wipe, |this| {
                this.child(note(
                    "Your settings, agents and other Buzz data stay on this device. Sign back \
                     in with this key and they’ll be here. Agents start only for this key."
                        .into(),
                ))
            });
        // `.buzz-dialog`: at most the window less 3rem, scrolling beyond.
        div()
            .id("sign-out-dialog")
            .max_h(window.viewport_size().height - px(48.))
            .overflow_y_scroll()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
            AlertDialog::new("Sign out of Buzz?")
                .description(
                    "Signing out removes your private key from this device. Your key is how you \
                     sign in, so save it before you continue. You’ll need it to sign back in as \
                     this identity.",
                )
                .child(self.key.clone())
                .child(options)
                // `<p role="alert">`, in body text.
                .children(self.error.clone().map(|error| div().text_body().child(error)))
                .action(
                    Button::new("sign-out-cancel", "Cancel")
                        .surface(Surface::Floating)
                        .disabled(pending)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(SignOutClosed))),
                )
                .action(
                    Button::new(
                        "sign-out-confirm",
                        if self.wipe {
                            "Sign out and erase"
                        } else {
                            "Sign out"
                        },
                    )
                    .surface(Surface::Floating)
                    .loading(pending)
                    .disabled(!ready || pending)
                    .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn confirm_waits_for_the_key_and_the_phrase() {
        assert!(!sign_out_ready(false, true, false, ""));
        assert!(!sign_out_ready(true, false, false, ""));
        assert!(sign_out_ready(true, true, false, ""));
        assert!(!sign_out_ready(true, true, true, "erase all"));
        assert!(sign_out_ready(true, true, true, " erase all my data "));
        assert!(!sign_out_ready(true, true, true, "Erase all my data"));
    }
}
