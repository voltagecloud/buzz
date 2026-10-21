//! One-time webhook secrets (`WorkflowWebhookSecrets.tsx`,
//! `WorkflowWebhookSecretDialog.tsx`). A save that first gave a workflow
//! its webhook trigger receives a secret the relay never shows again; this
//! dialog takes it from the session once and keeps it only while open.

use super::controls::{self, Confirm};
use crate::mirror::mirror;
use buzz_relay_client::workflows::{Action, Outcome, WorkflowOperation, Workflows};
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, Dialog, Surface, TypeStyled,
    tokens::{radius, space},
};
use gpui_kit::*;
use std::rc::Rc;
use std::sync::Arc;

/// Shown when leaving before revealing or copying the secret.
const CONTINUE_TITLE: &str = "Continue without this secret?";
const CONTINUE_DESCRIPTION: &str = "This private webhook secret cannot be recovered. Copy and store it before continuing, or explicitly leave it behind.";

struct Shown {
    event_id: String,
    workflow_id: String,
    secret: Option<String>,
    revealed: bool,
    handled: bool,
    confirming: bool,
    notice: Option<(String, bool)>,
}

pub struct SecretsView {
    workflows: Workflows,
    operations: Arc<[WorkflowOperation]>,
    shown: Option<Shown>,
    focus: FocusHandle,
    confirm_focus: FocusHandle,
    _watch: Task<()>,
}

impl SecretsView {
    pub fn new(workflows: Workflows, cx: &mut Context<Self>) -> Self {
        Self {
            operations: workflows.operations(),
            _watch: mirror(
                workflows.watch_operations(),
                cx,
                |this: &mut Self, value| {
                    this.operations = value;
                    this.sync();
                },
            ),
            workflows,
            shown: None,
            focus: cx.focus_handle(),
            confirm_focus: cx.focus_handle(),
        }
    }

    /// Shows the next held secret once the current one is done. Clearing
    /// or access loss withdraws the operation, which purges the value.
    fn sync(&mut self) {
        let active = self.shown.as_ref().is_some_and(|shown| {
            self.operations
                .iter()
                .any(|op| op.event_id == shown.event_id && op.outcome == Outcome::Succeeded)
        });
        if active {
            return;
        }
        self.shown = None;
        let Some(next) = self
            .operations
            .iter()
            .find(|op| {
                op.action == Action::Save && op.outcome == Outcome::Succeeded && op.secret_held
            })
            .cloned()
        else {
            return;
        };
        // The hand-off is single-shot.
        let secret = self.workflows.take_webhook_secret(&next.event_id);
        self.shown = Some(Shown {
            event_id: next.event_id,
            workflow_id: next.workflow.id,
            secret,
            revealed: false,
            handled: false,
            confirming: false,
            notice: None,
        });
    }

    fn leave(&mut self, cx: &mut Context<Self>) {
        let Some(shown) = &mut self.shown else { return };
        if shown.secret.is_none() || shown.handled {
            self.close(cx);
        } else {
            shown.confirming = true;
            cx.notify();
        }
    }

    /// Done with this secret: the next held one, if any, shows.
    fn close(&mut self, cx: &mut Context<Self>) {
        self.shown = None;
        self.sync();
        cx.notify();
    }

    fn copy(&mut self, value: String, secret: bool, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        if let Some(shown) = &mut self.shown {
            shown.notice = Some((
                if secret {
                    "Secret copied."
                } else {
                    "URL copied."
                }
                .into(),
                false,
            ));
            if secret {
                shown.handled = true;
            }
        }
        cx.notify();
    }
}

impl Render for SecretsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(shown) = &self.shown else {
            return div().into_any_element();
        };
        let c = *cx.colors();
        let hook = self.workflows.webhook_url(&shown.workflow_id);
        let code = |text: String| {
            div()
                .id(SharedString::from(format!("secret-code-{}", text.len())))
                .flex_1()
                .min_w(rems(12.))
                .overflow_x_scroll()
                .px(space::S3)
                .py(space::S2)
                .rounded(radius::CONTROL)
                .bg(c.surface_inset)
                .text_mono()
                .whitespace_nowrap()
                .child(text)
        };
        let label = |text: &'static str| div().text_label_sm().child(text);
        let row = || {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(space::S2)
                .min_w_0()
        };
        let mut body = div().flex().flex_col().gap(space::S2).min_w_0();
        body = body.child(label("Webhook URL"));
        body = match hook.clone() {
            Some(url) => body.child(
                row().mb(space::S3).child(code(url.clone())).child(
                    Button::new("secret-copy-url", "Copy URL")
                        .size(ButtonSize::Sm)
                        .surface(Surface::Floating)
                        .on_click(cx.listener(move |this, _, _, cx| this.copy(url.clone(), false, cx))),
                ),
            ),
            None => body.child(controls::note(
                format!(
                    "The relay’s HTTP address is unavailable from this host. Send requests to /hooks/{} on the relay.",
                    shown.workflow_id
                ),
                cx,
            )),
        };
        let secret = shown.secret.clone();
        let value = match (&secret, shown.revealed) {
            (None, _) => "Unavailable".to_string(),
            (Some(secret), true) => secret.clone(),
            (Some(_), false) => "•".repeat(24),
        };
        let copy_secret = secret.clone();
        body = body.child(label("X-Webhook-Secret header")).child(
            row()
                .child(code(value))
                .child(
                    Button::new(
                        "secret-reveal",
                        if shown.revealed { "Hide" } else { "Reveal" },
                    )
                    .size(ButtonSize::Sm)
                    .surface(Surface::Floating)
                    .disabled(secret.is_none())
                    .tooltip(if shown.revealed {
                        "Hide webhook secret"
                    } else {
                        "Reveal webhook secret"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(shown) = &mut this.shown {
                            shown.revealed = !shown.revealed;
                            shown.handled = true;
                        }
                        cx.notify();
                    })),
                )
                .child(
                    Button::new("secret-copy", "Copy secret")
                        .size(ButtonSize::Sm)
                        .surface(Surface::Floating)
                        .disabled(secret.is_none())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(secret) = copy_secret.clone() {
                                this.copy(secret, true, cx);
                            }
                        })),
                ),
        );
        if secret.is_none() {
            body = body.child(
                div()
                    .text_body_sm()
                    .text_color(c.text_danger)
                    .child("This secret is no longer available from this session."),
            );
        }
        if let Some((text, error)) = shown.notice.clone() {
            body = body.child(
                div()
                    .text_body_sm()
                    .text_color(if error { c.text_danger } else { c.text_subtle })
                    .child(text),
            );
        }
        let confirming = shown.confirming;
        let dialog = div()
            .track_focus(&self.focus)
            .key_context(buzz_ui::DIALOG_CONTEXT)
            .on_action(cx.listener(|this, _: &buzz_ui::Cancel, _, cx| this.leave(cx)))
            .child(
                Dialog::new("webhook-secret", "Webhook ready")
                    .description("This private secret is shown once and cannot be recovered. Copy and store it before continuing.")
                    .on_close(cx.listener(|this, _, _, cx| this.leave(cx)))
                    .footer(
                        Button::new("secret-continue", "Continue")
                            .prominent()
                            .surface(Surface::Floating)
                            .on_click(cx.listener(|this, _, _, cx| this.leave(cx))),
                    )
                    .child(body),
            );
        if !confirming && !self.focus.contains_focused(window, cx) {
            window.focus(&self.focus, cx);
        }
        let entity = cx.entity().downgrade();
        let back = entity.clone();
        let confirm = confirming.then(|| {
            Confirm::new(CONTINUE_TITLE, CONTINUE_DESCRIPTION, "Continue")
                .cancel("Go back")
                .layer(3)
                .render(
                    "secret-continue-confirm",
                    &self.confirm_focus,
                    Rc::new(move |_, cx| {
                        let _ = entity.update(cx, |this, cx| this.close(cx));
                    }),
                    Rc::new(move |_, cx| {
                        let _ = back.update(cx, |this, cx| {
                            if let Some(shown) = &mut this.shown {
                                shown.confirming = false;
                            }
                            cx.notify();
                        });
                    }),
                    window,
                    cx,
                )
        });
        div()
            .child(controls::layer(window, dialog.into_any_element(), 2))
            .children(confirm)
            .into_any_element()
    }
}
