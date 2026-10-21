//! Canvas reads and saves for one community (`capability.ts` `canvas`).
//!
//! Reads are fresh and, for anything a save depends on, strong. A save
//! checks the current head against the draft's base, then goes through
//! the durable outbox like any other write, and is confirmed by reading
//! its exact id back. A proven compare-and-swap refusal is dismissed from
//! the outbox and reported as [`CanvasError::Conflict`]; any uncertain
//! outcome stays in the outbox and blocks the next save in that channel.

use super::util::{CLOSED, now, with};
use super::{Message, Session};
use crate::canvas::{
    self, ACCESS_CHANGED, AWAITING, CanvasError, HistoryCursor, HistoryPage, OTHER_SELECTED,
    Revision, TOO_LARGE, TOO_OLD, TOO_SOON, UNAVAILABLE, UNRESOLVED, WRITE_UNAVAILABLE,
};
use crate::event::{Verified, kinds};
use crate::outbox::{Delivery, Outbox, SendOptions};
use crate::reader::{ReadOptions, Reader};
use crate::socket::Filter;
use buzz_identity::{Identity, Purpose};
use nostr::prelude::EventId;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// How long a save waits for its outbox delivery (`delivered`).
const DELIVERY_WAIT: Duration = Duration::from_secs(15);
const STILL_WAITING: &str =
    "Still waiting for confirmation. Retry without starting another session.";
const NOT_CONFIRMED: &str = "The operation could not be confirmed.";
/// An unconfirmed operation older than this is not replayed.
const REPLAY_SECS: u64 = 15 * 60;
/// Bounds a whole save: reads, delivery and readback.
const SAVE_TIMEOUT: Duration = Duration::from_secs(60);

/// What a Canvas call needs from the actor.
struct Access {
    outbox: Option<Outbox>,
    writable: bool,
}

/// The network side of Canvas calls, cloned off the session. The Me
/// settings and channel setup save through it too.
#[derive(Clone)]
pub(super) struct Canvas {
    commands: tokio::sync::mpsc::UnboundedSender<Message>,
    reader: Reader,
    identity: Arc<Identity>,
}

/// `outbox.supports(40100)`: this identity may sign Canvas saves.
fn supported(identity: &Identity) -> bool {
    identity
        .template(
            Purpose::Content,
            kinds::CANVAS,
            0,
            Vec::new(),
            String::new(),
        )
        .is_ok()
}

impl Canvas {
    pub(super) fn new(
        commands: tokio::sync::mpsc::UnboundedSender<Message>,
        reader: Reader,
        identity: Arc<Identity>,
    ) -> Self {
        Self {
            commands,
            reader,
            identity,
        }
    }

    /// `canWrite`: the session is open and the viewer may post in the
    /// channel, as the actor knows it now.
    async fn access(&self, channel: &str) -> Result<Access, CanvasError> {
        let channel = channel.to_owned();
        with(&self.commands, move |actor| Access {
            outbox: actor.outbox.clone(),
            writable: actor
                .discovery
                .as_ref()
                .is_some_and(|discovery| discovery.can_participate(&channel)),
        })
        .await
        .map_err(CanvasError::from)
    }

    async fn writable(&self, channel: &str) -> Result<bool, CanvasError> {
        Ok(self.access(channel).await?.writable)
    }

    async fn fresh(&self, filter: Filter) -> Result<crate::Events, CanvasError> {
        let options = ReadOptions {
            fresh: true,
            ..ReadOptions::default()
        };
        self.reader
            .read(vec![filter], options)
            .await
            .map_err(|error| CanvasError::Failed(error.message))
    }

    /// The selected head event, without an access check.
    pub(super) async fn head(
        &self,
        channel: &str,
        strong: bool,
    ) -> Result<Option<Verified>, CanvasError> {
        let events = self.fresh(canvas::head_filter(channel, strong)).await?;
        let Some(head) = canvas::selected_head(channel, &events) else {
            return Ok(None);
        };
        Ok(events.iter().find(|event| event.id == head.id).cloned())
    }

    /// The selected head event, if the viewer may still write here.
    pub(super) async fn read_event(
        &self,
        channel: &str,
        strong: bool,
    ) -> Result<Option<Verified>, CanvasError> {
        if !self.writable(channel).await? {
            return Err(UNAVAILABLE.into());
        }
        self.head(channel, strong).await
    }

    async fn read(&self, channel: &str, strong: bool) -> Result<Option<Revision>, CanvasError> {
        Ok(self
            .read_event(channel, strong)
            .await?
            .map(|event| Revision::from_event(event.event())))
    }

    async fn history(
        &self,
        channel: &str,
        cursor: Option<HistoryCursor>,
    ) -> Result<HistoryPage, CanvasError> {
        if !self.writable(channel).await? {
            return Err(UNAVAILABLE.into());
        }
        let rows = self
            .fresh(canvas::history_filter(channel, cursor.as_ref()))
            .await?;
        if !self.writable(channel).await? {
            return Err(UNAVAILABLE.into());
        }
        canvas::history_page(channel, cursor.as_ref(), &rows)
    }

    async fn found(&self, id: EventId) -> Result<bool, CanvasError> {
        let filter = json!({"ids": [id.to_hex()], "limit": 1, "consistency": "strong"});
        let serde_json::Value::Object(filter) = filter else {
            unreachable!("filters are objects")
        };
        Ok(self.fresh(filter).await?.iter().any(|event| event.id == id))
    }

    async fn save(
        &self,
        channel: &str,
        content: &str,
        expected: Option<EventId>,
    ) -> Result<Revision, CanvasError> {
        let saved = self.save_event(channel, content, expected).await?;
        Ok(Revision::from_event(saved.event()))
    }

    /// Saves and returns the confirmed head event.
    pub(super) async fn save_event(
        &self,
        channel: &str,
        content: &str,
        expected: Option<EventId>,
    ) -> Result<Verified, CanvasError> {
        let access = self.access(channel).await?;
        let outbox = match access.outbox {
            Some(outbox) if access.writable && supported(&self.identity) => outbox,
            _ => return Err(WRITE_UNAVAILABLE.into()),
        };
        if canvas::too_large(content) {
            return Err(TOO_LARGE.into());
        }
        let draft = canvas::save_draft(channel, content, expected.as_ref())?;
        outbox
            .ready()
            .await
            .map_err(|error| CanvasError::Failed(error.to_string()))?;
        let head = self.read_event(channel, true).await?;
        if head.as_ref().map(|head| head.id) != expected {
            return Err(CanvasError::Conflict);
        }
        if outbox
            .pending()
            .iter()
            .any(|op| canvas::is_unresolved(op, channel))
        {
            return Err(UNRESOLVED.into());
        }
        if head.is_some_and(|head| head.created_at.as_secs() >= now()) {
            return Err(TOO_SOON.into());
        }
        if !self.writable(channel).await? {
            return Err(ACCESS_CHANGED.into());
        }
        let id = outbox
            .send(draft, SendOptions::default())
            .map_err(|error| CanvasError::Failed(error.to_string()))?;
        self.confirm(&outbox, id).await?;
        match self.read_event(channel, true).await? {
            Some(selected) if selected.id == id => Ok(selected),
            _ => Err(OTHER_SELECTED.into()),
        }
    }

    /// `confirm`: the exact save is on the relay. A proven refusal of its
    /// precondition is dismissed so a reviewed save can follow.
    pub(super) async fn confirm(&self, outbox: &Outbox, id: EventId) -> Result<(), CanvasError> {
        let result = self.confirm_inner(outbox, id).await;
        if result.is_err()
            && outbox
                .get(&id)
                .is_some_and(|op| canvas::is_definitive_conflict(&op))
        {
            // Refused before mutation: release the gate it holds.
            outbox
                .dismiss(&id)
                .await
                .map_err(|error| CanvasError::Failed(error.to_string()))?;
            return Err(CanvasError::Conflict);
        }
        result
    }

    async fn confirm_inner(&self, outbox: &Outbox, id: EventId) -> Result<(), CanvasError> {
        if self.found(id).await? {
            return Ok(());
        }
        let Some(saved) = outbox.get(&id) else {
            return Err(TOO_OLD.into());
        };
        if now().saturating_sub(saved.created_at()) >= REPLAY_SECS {
            return Err(TOO_OLD.into());
        }
        // Replaying these bytes cannot make a proven refusal valid.
        if canvas::is_definitive_conflict(&saved) {
            return Err(CanvasError::Failed(saved.error.clone().unwrap_or_default()));
        }
        self.delivered(outbox, id).await?;
        if !self.found(id).await? {
            return Err(AWAITING.into());
        }
        Ok(())
    }

    /// `delivered`: waits for the outbox to settle the operation,
    /// republishing the same signed bytes if an earlier attempt failed.
    async fn delivered(&self, outbox: &Outbox, id: EventId) -> Result<(), CanvasError> {
        if outbox
            .get(&id)
            .is_some_and(|op| matches!(op.delivery, Delivery::Failed | Delivery::Unknown))
        {
            outbox
                .retry(&id, None)
                .map_err(|error| CanvasError::Failed(error.to_string()))?;
        }
        let mut changes = outbox.changes();
        let settled = async {
            loop {
                match outbox.get(&id) {
                    Some(op) if matches!(op.delivery, Delivery::Accepted | Delivery::Seen) => {
                        return Ok(());
                    }
                    Some(op) if matches!(op.delivery, Delivery::Failed | Delivery::Unknown) => {
                        return Err(CanvasError::Failed(
                            op.error.clone().unwrap_or_else(|| NOT_CONFIRMED.into()),
                        ));
                    }
                    Some(_) => {}
                    None => return Err(NOT_CONFIRMED.into()),
                }
                if changes.changed().await.is_err() {
                    return Err(CLOSED.into());
                }
            }
        };
        tokio::time::timeout(DELIVERY_WAIT, settled)
            .await
            .unwrap_or_else(|_| Err(STILL_WAITING.into()))
    }
}

impl Session {
    pub(super) fn canvas(&self) -> Canvas {
        Canvas::new(
            self.commands.clone(),
            self.direct.reader.clone(),
            self.direct.identity.clone(),
        )
    }

    async fn on_io<R: Send + 'static>(
        &self,
        timeout: Duration,
        work: impl Future<Output = Result<R, CanvasError>> + Send + 'static,
    ) -> Result<R, CanvasError> {
        self.runtime
            .spawn(async move {
                tokio::time::timeout(timeout, work)
                    .await
                    .unwrap_or_else(|_| Err(AWAITING.into()))
            })
            .await
            .unwrap_or_else(|_| Err(CLOSED.into()))
    }

    /// Whether Canvas saves are possible in this community at all
    /// (`canvas.available`): the session has connected and this identity
    /// signs Canvas saves. Each call still checks channel access.
    pub async fn canvas_available(&self) -> bool {
        with(&self.commands, |actor| actor.outbox.is_some())
            .await
            .unwrap_or(false)
            && supported(&self.direct.identity)
    }

    /// The channel's current Canvas, `None` when it has none. Editors and
    /// save bases read `strong`; display-only reads may use a replica.
    pub async fn canvas_read(
        &self,
        channel: &str,
        strong: bool,
    ) -> Result<Option<Revision>, CanvasError> {
        let (canvas, channel) = (self.canvas(), channel.to_owned());
        self.on_io(
            SAVE_TIMEOUT,
            async move { canvas.read(&channel, strong).await },
        )
        .await
    }

    /// One page of saved revisions, newest first: the first page (no
    /// cursor) is strong, older pages follow `page.next`.
    pub async fn canvas_history(
        &self,
        channel: &str,
        cursor: Option<HistoryCursor>,
    ) -> Result<HistoryPage, CanvasError> {
        let (canvas, channel) = (self.canvas(), channel.to_owned());
        self.on_io(SAVE_TIMEOUT, async move {
            canvas.history(&channel, cursor).await
        })
        .await
    }

    /// Replaces the Canvas with `content`. `expected` is the id of the
    /// revision the draft started from (`None`: there was no Canvas). A
    /// different current head, or the relay's own compare-and-swap
    /// refusal, is [`CanvasError::Conflict`]. Returns the saved revision,
    /// confirmed as the channel's current Canvas. Any other error may
    /// follow a delivered save: keep the draft and reload to check.
    ///
    /// Restoring a history revision is a save of its content with the
    /// displayed head as `expected`.
    pub async fn canvas_save(
        &self,
        channel: &str,
        content: &str,
        expected: Option<EventId>,
    ) -> Result<Revision, CanvasError> {
        let (canvas, channel, content) = (self.canvas(), channel.to_owned(), content.to_owned());
        self.on_io(SAVE_TIMEOUT, async move {
            canvas.save(&channel, &content, expected).await
        })
        .await
    }
}
