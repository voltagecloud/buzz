//! Live events of any kind for in-app consumers (buzz-app's
//! `session.subscribeLive`), such as Agents2, which wakes agents from the
//! stream the owner already receives instead of opening its own.

use super::{Actor, Message, Session};
use crate::event::Verified;
use nostr::prelude::Event;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Verified live-phase events from one delivery. `channel` is the route's
/// channel when the wire scope named exactly one; global routes omit it.
#[derive(Clone, Debug)]
pub struct LiveBatch {
    pub events: Vec<Arc<Event>>,
    pub channel: Option<String>,
}

impl Session {
    /// Receives live-phase events (never replay, finite reads or local
    /// intent) after reconciliation, from now on, including the viewer's
    /// own echoes. Overlapping routes can deliver an event more than once,
    /// so receivers dedupe by id. Dropping the receiver unsubscribes.
    pub fn live_events(&self) -> mpsc::UnboundedReceiver<LiveBatch> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor.live_feed.push(sender);
        })));
        receiver
    }
}

impl Actor {
    pub(super) fn feed_live(&mut self, events: &[Verified], channel: Option<&str>) {
        if self.live_feed.is_empty() || events.is_empty() {
            return;
        }
        let batch = LiveBatch {
            events: events
                .iter()
                .map(|event| std::sync::Arc::new(event.event().clone()))
                .collect(),
            channel: channel.map(str::to_owned),
        };
        self.live_feed
            .retain(|sender| sender.send(batch.clone()).is_ok());
    }
}
