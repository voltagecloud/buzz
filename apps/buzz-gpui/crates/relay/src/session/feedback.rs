//! Product feedback through the session's durable outbox (`session.ts`
//! `feedbackUpload`, `FeedbackDialog.tsx`). At most one feedback event is
//! outstanding; it stays in the outbox, delivered or not, until the user
//! clears it, so the dialog can show its state after a restart.

use super::{Message, Session, SessionStatus};
use crate::feedback::{self, Category, PRODUCT_FEEDBACK_KIND};
use crate::messages::Attachment;
use crate::outbox::{Delivery, Outbox, SendOptions};
use crate::upload::{self, UploadError, UploadFile};
use buzz_identity::{Identity, Purpose};
use nostr::prelude::EventId;
use tokio::sync::{oneshot, watch};

const UNAVAILABLE: &str = "Connect to a Buzz deployment to send or retry feedback.";

/// The feedback event waiting in the outbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingFeedback {
    pub id: EventId,
    pub content: String,
    pub delivery: Delivery,
    pub error: Option<String>,
}

/// What the feedback dialog shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedbackState {
    /// The outbox finished loading its saved operations.
    pub hydrated: bool,
    /// The session is ready, the outbox loaded and this identity may
    /// write feedback: sending and retrying are possible.
    pub available: bool,
    /// The saved operations could not be loaded.
    pub error: Option<String>,
    pub pending: Option<PendingFeedback>,
}

fn pending(outbox: &Outbox) -> Option<PendingFeedback> {
    outbox
        .pending()
        .iter()
        .find(|op| op.kind() == PRODUCT_FEEDBACK_KIND)
        .map(|op| PendingFeedback {
            id: op.id(),
            content: op.event.content.clone(),
            delivery: op.delivery,
            error: op.error.clone(),
        })
}

/// `outbox.supports(42000)`: this identity may sign the kind.
fn supported(identity: &Identity) -> bool {
    identity
        .template(
            Purpose::Content,
            PRODUCT_FEEDBACK_KIND,
            0,
            Vec::new(),
            String::new(),
        )
        .is_ok()
}

impl Session {
    /// The outbox, once the session has connected.
    async fn feedback_outbox(&self) -> Result<Outbox, String> {
        super::util::with(&self.commands, |actor| actor.outbox.clone())
            .await?
            .ok_or_else(|| UNAVAILABLE.to_owned())
    }

    /// Follows the pending feedback and whether feedback can be sent. The
    /// watch ends when every receiver is dropped.
    pub fn feedback(&self) -> watch::Receiver<FeedbackState> {
        let (sender, receiver) = watch::channel(FeedbackState::default());
        let commands = self.commands.clone();
        let mut session = self.snapshot();
        let supported = supported(&self.direct.identity);
        self.runtime.spawn(async move {
            let ready = |session: &watch::Receiver<std::sync::Arc<super::SessionSnapshot>>| {
                session.borrow().status == SessionStatus::Ready
            };
            // The outbox exists once the session first connects.
            let outbox = loop {
                let (reply, answer) = oneshot::channel();
                if commands
                    .send(Message::With(Box::new(move |actor| {
                        let _ = reply.send(actor.outbox.clone());
                    })))
                    .is_err()
                {
                    return;
                }
                match answer.await {
                    Ok(Some(outbox)) => break outbox,
                    Ok(None) => {}
                    Err(_) => return,
                }
                tokio::select! {
                    changed = session.changed() => if changed.is_err() { return },
                    () = sender.closed() => return,
                }
            };
            let loaded = tokio::select! {
                loaded = outbox.ready() => loaded,
                () = sender.closed() => return,
            };
            let error = loaded.err().map(|error| error.to_string());
            let mut changes = outbox.changes();
            loop {
                let state = FeedbackState {
                    hydrated: true,
                    available: error.is_none() && supported && ready(&session),
                    error: error.clone(),
                    pending: pending(&outbox),
                };
                sender.send_if_modified(|current| {
                    let changed = *current != state;
                    *current = state;
                    changed
                });
                tokio::select! {
                    changed = changes.changed() => if changed.is_err() { return },
                    changed = session.changed() => if changed.is_err() { return },
                    () = sender.closed() => return,
                }
            }
        });
        receiver
    }

    /// Uploads a feedback attachment to the community's ordinary media
    /// storage. Images lose their metadata on the way.
    pub async fn upload_feedback(&self, file: UploadFile) -> Result<Attachment, UploadError> {
        upload::upload(self.media(), file).await
    }

    /// Queues the feedback event in the durable outbox. Refused while an
    /// earlier feedback event is still outstanding.
    pub async fn send_feedback(
        &self,
        message: &str,
        category: Option<Category>,
        attachments: &[Attachment],
    ) -> Result<EventId, String> {
        let draft = feedback::event(message, category, attachments, self.origin())?;
        let outbox = self.feedback_outbox().await?;
        if self.snapshot.borrow().status != SessionStatus::Ready
            || !supported(&self.direct.identity)
        {
            return Err(UNAVAILABLE.to_owned());
        }
        self.run(async move {
            outbox.ready().await.map_err(|error| error.to_string())?;
            if pending(&outbox).is_some() {
                return Err("Earlier feedback is still waiting. Retry or clear it first.".into());
            }
            outbox
                .send(draft, SendOptions::default())
                .map_err(|error| error.to_string())
        })
        .await
    }

    /// Republishes the same signed feedback event.
    pub async fn retry_feedback(&self, id: EventId) -> Result<(), String> {
        let outbox = self.feedback_outbox().await?;
        if self.snapshot.borrow().status != SessionStatus::Ready {
            return Err(UNAVAILABLE.to_owned());
        }
        self.run(async move {
            outbox.ready().await.map_err(|error| error.to_string())?;
            outbox.retry(&id, None).map_err(|error| error.to_string())
        })
        .await
    }

    /// Forgets the feedback event locally. Delivery, if it happened, and
    /// uploaded attachments are not undone.
    pub async fn dismiss_feedback(&self, id: EventId) -> Result<(), String> {
        let outbox = self.feedback_outbox().await?;
        self.run(async move { outbox.dismiss(&id).await.map_err(|error| error.to_string()) })
            .await
    }
}
