//! Request and reply exchanges on the live socket, such as kind 24210
//! Sandbox session control: ephemeral events, which nothing queues, stores
//! or retries.

use super::{Message, Session};
use crate::socket::{Filter, SocketPublisher, Subscription};
use tokio::sync::oneshot;

impl Session {
    /// A subscription to `filters` and a publisher, both on the live
    /// socket. Fails without a live connection.
    pub async fn live_exchange(
        &self,
        filters: Vec<Filter>,
    ) -> Result<(Subscription, SocketPublisher), String> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Message::With(Box::new(move |actor| {
                let socket = actor.live.as_ref().map(|live| live.socket());
                let _ = reply
                    .send(socket.map(|socket| (socket.subscribe(filters), socket.publisher())));
            })))
            .map_err(|_| "The session has closed".to_string())?;
        answer
            .await
            .map_err(|_| "The session has closed".to_string())?
            .ok_or_else(|| "This needs a live connection to the community".into())
    }
}
