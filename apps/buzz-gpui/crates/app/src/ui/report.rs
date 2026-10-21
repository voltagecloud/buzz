//! The report dialog (`ReportMessageDialog.tsx`): a reason, optional
//! context for the moderators, and a submit that is kept until it settles.

use buzz_relay_client::messages::ReportType;
use buzz_relay_client::{EventId, Session};
use buzz_ui::{
    ActiveTheme, Button, ButtonSize, ButtonVariant, Dialog, IconName, Radio, TextArea, TypeStyled,
};
use gpui_kit::component::input::TextareaState;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;

/// Same order and copy as Buzz desktop; "Other" reads as the fallback.
pub const CATEGORIES: [(ReportType, &str); 7] = [
    (ReportType::Spam, "Spam"),
    (ReportType::Profanity, "Profanity or hate speech"),
    (ReportType::Nudity, "Nudity or sexual content"),
    (ReportType::Impersonation, "Impersonation"),
    (ReportType::Malware, "Malware or scam"),
    (ReportType::Illegal, "Illegal content"),
    (ReportType::Other, "Other"),
];

pub const FAILED: &str = "Failed to submit report. Try again.";
pub const SUBMITTED: &str = "Report submitted to community moderators";

/// The dialog closed, submitted or not.
pub struct ReportClosed;

pub struct ReportDialog {
    session: Arc<Session>,
    channel: String,
    message: EventId,
    category: Option<ReportType>,
    note: Entity<TextareaState>,
    pending: bool,
    error: Option<SharedString>,
    focus: FocusHandle,
}

impl EventEmitter<ReportClosed> for ReportDialog {}

impl ReportDialog {
    pub fn new(
        session: Arc<Session>,
        channel: String,
        message: EventId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let note = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder("Add anything that helps moderators...")
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            session,
            channel,
            message,
            category: None,
            note,
            pending: false,
            error: None,
            focus,
        }
    }

    pub fn pending(&self) -> bool {
        self.pending
    }

    /// The last submission's failure, until the next one.
    pub fn error(&self) -> Option<SharedString> {
        self.error.clone()
    }

    pub fn message(&self) -> EventId {
        self.message
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        // A dispatched report must settle; only unsent ones are discarded.
        if !self.pending {
            cx.emit(ReportClosed);
        }
    }

    /// Submits; the task is detached so a report already sent settles
    /// even if the conversation closes.
    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(category) = self.category else {
            return;
        };
        if self.pending {
            return;
        }
        self.pending = true;
        self.error = None;
        let note = self.note.read(cx).value().to_string();
        let (session, channel, message) =
            (self.session.clone(), self.channel.clone(), self.message);
        cx.spawn(async move |this, cx| {
            let result = session.report(&channel, message, category, &note).await;
            let accepted = result.is_ok();
            let _ = this.update(cx, |this, cx| {
                this.pending = false;
                match result {
                    Ok(()) => cx.emit(ReportClosed),
                    Err(error) => {
                        tracing::warn!(%error, "report failed");
                        this.error = Some(FAILED.into());
                    }
                }
                cx.notify();
            });
            if accepted {
                cx.update(|cx| {
                    super::notice::show(
                        SUBMITTED,
                        buzz_ui::ToastTone::Success,
                        Some(std::time::Duration::from_secs(5)),
                        cx,
                    )
                });
            }
        })
        .detach();
        cx.notify();
    }
}

impl Render for ReportDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let pending = self.pending;
        let mut reasons = div().flex().flex_col().gap(px(12.));
        for (index, (kind, label)) in CATEGORIES.iter().enumerate() {
            let kind = *kind;
            reasons = reasons.child(
                Radio::new(
                    ("report-reason", index),
                    *label,
                    self.category == Some(kind),
                )
                .disabled(pending)
                .on_select({
                    let this = cx.entity().downgrade();
                    move |_, cx| {
                        let _ = this.update(cx, |this, cx| {
                            this.category = Some(kind);
                            cx.notify();
                        });
                    }
                }),
            );
        }
        let field = |label: &str| {
            div()
                .text_label_sm()
                .text_color(c.text_standard)
                .child(label.to_string())
        };
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.close(cx);
                }
            }))
            .child(
                Dialog::new("report-dialog", "Report message")
                    .title_icon(IconName::Flag)
                    .description(
                        "Reports go to this community's moderators for review. The author is \
                         not notified of who reported them.",
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(16.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(8.))
                                    .child(field("Reason"))
                                    .child(reasons)
                                    .when_some(self.error.clone(), |d, error| {
                                        d.child(
                                            div()
                                                .text_body_sm()
                                                .text_color(c.text_danger)
                                                .child(error),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(8.))
                                    .child(field("Additional context (optional)"))
                                    .child(TextArea::new(&self.note).rows(3).disabled(pending)),
                            ),
                    )
                    .footer(
                        Button::new("report-cancel", "Cancel")
                            .size(ButtonSize::Md)
                            .disabled(pending)
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    )
                    .footer(
                        Button::new("report-submit", "Submit report")
                            .variant(ButtonVariant::Prominent)
                            .size(ButtonSize::Md)
                            .disabled(self.category.is_none())
                            .loading(pending)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
    }
}
