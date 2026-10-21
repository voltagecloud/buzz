//! Small helpers the session's operations share.

use super::{Actor, Message};
use crate::http::{HttpError, RelayHttp};
use crate::messages::EventTemplate;
use crate::outbox::{PublishError, accept_publish};
use buzz_identity::{Identity, Purpose};
use nostr::prelude::Event;
use tokio::sync::{mpsc, oneshot};

pub(super) const CLOSED: &str = "The session has closed";
pub(super) const NOT_CONNECTED: &str = "Not connected to the relay yet";

/// Now, in Unix seconds.
pub(super) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Runs `f` on the actor and hands back its result.
pub(super) async fn with<R: Send + 'static>(
    commands: &mpsc::UnboundedSender<Message>,
    f: impl FnOnce(&mut Actor) -> R + Send + 'static,
) -> Result<R, String> {
    let (reply, answer) = oneshot::channel();
    commands
        .send(Message::With(Box::new(move |actor| {
            let _ = reply.send(f(actor));
        })))
        .map_err(|_| CLOSED.to_owned())?;
    answer.await.map_err(|_| CLOSED.to_owned())
}

/// Signs `template` with the viewer's content key.
pub(super) fn sign(identity: &Identity, template: EventTemplate) -> Result<Event, String> {
    let unsigned = identity
        .template(
            Purpose::Content,
            template.kind,
            template.created_at,
            template.tags,
            template.content,
        )
        .map_err(|error| error.to_string())?;
    identity
        .sign(Purpose::Content, unsigned)
        .map_err(|error| error.to_string())
}

/// Posts a signed event straight to the relay (`POST /events`).
pub(super) async fn post_event(http: &RelayHttp, event: &Event) -> Result<Vec<u8>, HttpError> {
    let body = serde_json::to_string(event).expect("events serialize");
    http.post("/events", body).await
}

/// Posts a signed event and checks its receipt. A refusal and an
/// uncertain outcome both become the relay's message.
pub(super) async fn publish(http: &RelayHttp, event: &Event) -> Result<(), String> {
    accept_publish(post_event(http, event).await, &event.id)
        .map(drop)
        .map_err(|error| match error {
            PublishError::Rejected(message) | PublishError::Uncertain(message) => message,
        })
}
