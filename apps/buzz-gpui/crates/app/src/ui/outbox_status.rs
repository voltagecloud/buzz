//! The session's outbox in channel settings' Diagnostics (`OutboxStatus`):
//! every outstanding send, from any page or a previous launch, with Retry
//! and "Remove from outbox".

use buzz_relay_client::outbox::{Delivery, Operation, Outbox, is_workflow_operation};
use buzz_relay_client::{EventId, Session, kinds};
use buzz_ui::{ActiveTheme, Button, ButtonSize, Icon, IconName, TypeStyled, tokens::space};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::Arc;
use std::time::Duration;

/// Product feedback travels through the outbox but is not a send to show.
const PRODUCT_FEEDBACK: u16 = buzz_relay_client::feedback::PRODUCT_FEEDBACK_KIND;

pub struct OutboxStatus {
    session: Arc<Session>,
    operations: Arc<[Arc<Operation>]>,
    open: bool,
    error: Option<String>,
    _watch: Task<()>,
}

impl OutboxStatus {
    pub fn new(session: Arc<Session>, cx: &mut Context<Self>) -> Self {
        let watched = session.clone();
        let watch = cx.spawn(async move |this, cx| {
            // A reconnect replaces the outbox; follow the current one.
            loop {
                let Some(outbox) = watched.outbox().await else {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    continue;
                };
                let mut changes = outbox.changes();
                loop {
                    if this.update(cx, |this, cx| this.read(&outbox, cx)).is_err() {
                        return;
                    }
                    if changes.changed().await.is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            session,
            operations: Arc::from([]),
            open: false,
            error: None,
            _watch: watch,
        }
    }

    fn read(&mut self, outbox: &Outbox, cx: &mut Context<Self>) {
        self.operations = outbox
            .pending()
            .iter()
            .filter(|op| op.kind() != PRODUCT_FEEDBACK)
            .cloned()
            .collect();
        cx.notify();
    }

    fn retry(&mut self, id: EventId, cx: &mut Context<Self>) {
        let session = self.session.clone();
        cx.spawn(async move |_, _| {
            // The row's own delivery state reports the outcome.
            let _ = session.retry_send(id).await;
        })
        .detach();
    }

    fn remove(&mut self, id: EventId, cx: &mut Context<Self>) {
        let session = self.session.clone();
        cx.spawn(async move |this, cx| {
            let result = session.dismiss(id).await;
            let _ = this.update(cx, |this, cx| {
                this.error = result.err();
                cx.notify();
            });
        })
        .detach();
    }
}

/// What a row says: an agent addition, or the first 100 characters.
fn excerpt(op: &Operation) -> String {
    if op.kind() == kinds::PUT_USER {
        let key: String = op.tag("p").unwrap_or_default().chars().take(12).collect();
        format!("Add agent {key}")
    } else {
        op.event.content.chars().take(100).collect()
    }
}

fn delivery_label(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::Sending => "Sending",
        Delivery::Accepted | Delivery::Seen => "Sent",
        Delivery::Unknown => "Unconfirmed",
        Delivery::Failed => "Not sent",
    }
}

/// Retry is offered for a failed or unconfirmed send, except workflow
/// commands and guarded operations, which retry from where they started.
fn retryable(op: &Operation) -> bool {
    matches!(op.delivery, Delivery::Failed | Delivery::Unknown)
        && !op.guarded
        && !is_workflow_operation(&op.event)
}

impl Render for OutboxStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let summary = div()
            .id("outbox-summary")
            .flex()
            .items_center()
            .gap(space::S1)
            .cursor_pointer()
            .child(Icon::new(if self.open {
                IconName::CaretDown
            } else {
                IconName::CaretRight
            }))
            .child(format!("Outbox · {} items", self.operations.len()))
            .on_click(cx.listener(|this, _, _, cx| {
                this.open = !this.open;
                cx.notify();
            }));
        let body = self.open.then(|| {
            let rows = self.operations.iter().enumerate().map(|(index, op)| {
                let id = op.id();
                let unsettled = matches!(op.delivery, Delivery::Failed | Delivery::Unknown);
                // The text wraps; the buttons follow on their own line.
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(space::S2)
                    .child(div().w_full().child(format!(
                        "{} · {}",
                        excerpt(op),
                        delivery_label(op.delivery)
                    )))
                    .when(retryable(op), |row| {
                        row.child(
                            Button::new(("outbox-retry", index), "Retry")
                                .size(ButtonSize::Sm)
                                .on_click(cx.listener(move |this, _, _, cx| this.retry(id, cx))),
                        )
                    })
                    .when(op.guarded && unsettled, |row| {
                        row.child(
                            "Retry from the channel or agent profile after checking membership.",
                        )
                    })
                    .when(op.delivery != Delivery::Sending, |row| {
                        row.child(
                            Button::new(("outbox-remove", index), "Remove from outbox")
                                .size(ButtonSize::Sm)
                                .on_click(cx.listener(move |this, _, _, cx| this.remove(id, cx))),
                        )
                    })
            });
            div()
                .flex()
                .flex_col()
                .gap(space::S2)
                .pt(space::S2)
                .child("Removing an item from the outbox does not delete a sent message.")
                .child(
                    div()
                        .id("outbox-items")
                        .flex()
                        .flex_col()
                        .gap(space::S2)
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .pl(space::S4)
                        .children(rows),
                )
                .children(
                    self.error
                        .clone()
                        .map(|error| div().text_color(c.text_danger).child(error)),
                )
        });
        div()
            .flex()
            .flex_col()
            .text_body_sm()
            .child(summary)
            .children(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use nostr::prelude::{Kind, Tag, Timestamp, UnsignedEvent};

    fn op(kind: u16, content: &str, tags: Vec<Vec<&str>>, delivery: Delivery) -> Operation {
        let key = buzz_relay_client::PublicKey::from_slice(&[2; 32]).unwrap();
        let tags = tags
            .into_iter()
            .map(|t| Tag::parse(t).unwrap())
            .collect::<Vec<_>>();
        let mut event =
            UnsignedEvent::new(key, Timestamp::from(1), Kind::from(kind), tags, content);
        event.ensure_id();
        Operation {
            event,
            signed: None,
            recovery: None,
            acknowledged: false,
            guarded: false,
            delivery,
            error: None,
        }
    }

    #[test]
    fn rows_name_agents_and_trim_text() {
        let long = "x".repeat(150);
        assert_eq!(excerpt(&op(9, &long, vec![], Delivery::Failed)).len(), 100);
        let key = "ab".repeat(32);
        let add = op(kinds::PUT_USER, "", vec![vec!["p", &key]], Delivery::Failed);
        assert_eq!(excerpt(&add), "Add agent abababababab");
        assert_eq!(delivery_label(Delivery::Seen), "Sent");
        assert_eq!(delivery_label(Delivery::Unknown), "Unconfirmed");
    }

    #[test]
    fn retry_skips_settled_guarded_and_workflow_sends() {
        assert!(retryable(&op(9, "hi", vec![], Delivery::Failed)));
        assert!(retryable(&op(9, "hi", vec![], Delivery::Unknown)));
        assert!(!retryable(&op(9, "hi", vec![], Delivery::Accepted)));
        let mut guarded = op(9, "hi", vec![], Delivery::Failed);
        guarded.guarded = true;
        assert!(!retryable(&guarded));
        assert!(!retryable(&op(46020, "{}", vec![], Delivery::Failed)));
    }
}
