//! Open thread panels, driven as `crate::threads` describes.

use super::{Actor, Message};
use crate::Events;
use crate::event::Verified;
use crate::fold::LocalDelivery;
use crate::reader::{ReadError, ReadErrorKind};
use crate::threads::{
    Retarget, ThreadConfig, ThreadRead, ThreadSnapshot, ThreadState, thread_authority,
};
use nostr::prelude::{EventId, PublicKey};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};

/// Thread views one session may own.
const MAX_THREAD_VIEWS: usize = 64;

/// An open thread panel. Dropping it disposes the view.
pub struct ThreadView {
    pub snapshot: watch::Receiver<Arc<ThreadSnapshot>>,
    pub(super) id: u64,
    pub(super) commands: mpsc::UnboundedSender<Message>,
}

impl ThreadView {
    pub fn load_more(&self) {
        let _ = self.commands.send(Message::ThreadMore(self.id));
    }

    pub fn refresh(&self) {
        let _ = self.commands.send(Message::ThreadRefresh(self.id));
    }

    /// Selects `message` in this open thread without reloading it
    /// ([`ThreadState::retarget`]). The snapshot is published before the
    /// outcome arrives. A later call, access loss, a cache clear or
    /// closing the view answers [`Retarget::Cancelled`].
    pub fn retarget(&self, message: EventId) -> oneshot::Receiver<Retarget> {
        let (reply, outcome) = oneshot::channel();
        let id = self.id;
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor.thread_retarget(id, message, reply)
        })));
        outcome
    }

    /// Reports what the panel shows, as [`super::ChannelView::viewport`]
    /// does for a channel: the replies fully in view (at most 128) and
    /// whether the panel sits at its newest reply. Visible replies that
    /// stay in view for [`super::READING_DWELL_MS`] are read, and at the
    /// bottom the thread is caught up. Report nothing and `false` while the
    /// window or the panel is not focused.
    pub fn viewport(&self, visible: Vec<EventId>, at_bottom: bool) {
        let id = self.id;
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            actor.thread_viewport(id, visible, at_bottom)
        })));
    }
}

impl Drop for ThreadView {
    fn drop(&mut self) {
        let _ = self.commands.send(Message::CloseThread(self.id));
    }
}

/// A thread view opened before the relay identified itself.
pub(super) struct WaitingThread {
    channel: String,
    message: EventId,
    exact: bool,
    view: watch::Sender<Arc<ThreadSnapshot>>,
}

pub(super) struct OpenThread {
    pub state: ThreadState,
    pub channel: String,
    view: watch::Sender<Arc<ThreadSnapshot>>,
    /// Answers the retarget in flight.
    retarget: Option<oneshot::Sender<Retarget>>,
}

impl OpenThread {
    /// What the panel shows now.
    pub fn snapshot(&self) -> Arc<ThreadSnapshot> {
        self.view.borrow().clone()
    }

    fn publish(&mut self) {
        self.settle(None);
    }

    /// Publishes, then answers the retarget in flight with `outcome`, or
    /// as cancelled once the state dropped it.
    fn settle(&mut self, outcome: Option<Retarget>) {
        if let Some(snapshot) = self.state.take_snapshot() {
            self.view.send_replace(snapshot);
        }
        let outcome = outcome.or_else(|| (!self.state.selecting()).then_some(Retarget::Cancelled));
        if let Some(outcome) = outcome
            && let Some(reply) = self.retarget.take()
        {
            let _ = reply.send(outcome);
        }
    }
}

impl Actor {
    /// A channel's admins changed: its open threads refold their rows.
    pub(super) fn threads_set_admins(&mut self, channel: &str, admins: &HashSet<PublicKey>) {
        for thread in self.threads.values_mut() {
            if thread.channel == channel && thread.state.set_admins(admins) {
                thread.publish();
            }
        }
    }

    /// An event an open thread view retains, as visibility evidence.
    pub(super) fn thread_event(&self, id: &EventId) -> Option<Verified> {
        self.threads.values().find_map(|thread| {
            let accessible = self
                .discovery
                .as_ref()
                .is_some_and(|discovery| discovery.can_access(&thread.channel));
            accessible
                .then(|| thread.state.event(id).cloned())
                .flatten()
        })
    }

    pub(super) fn open_thread(
        &mut self,
        id: u64,
        channel: String,
        message: EventId,
        exact: bool,
        view: watch::Sender<Arc<ThreadSnapshot>>,
    ) {
        // At most 64 views a session owns (`session.ts` `thread`).
        self.threads.retain(|_, thread| !thread.view.is_closed());
        self.waiting_threads
            .retain(|_, waiting| !waiting.view.is_closed());
        if self.threads.len() + self.waiting_threads.len() >= MAX_THREAD_VIEWS {
            let mut refused = ThreadSnapshot::idle(exact);
            refused.status = crate::threads::ThreadStatus::Error;
            refused.error = Some("Relay view capacity unavailable".into());
            view.send_replace(Arc::new(refused));
            return;
        }
        let Some(relay) = self.relay_author() else {
            // Opened once the relay answers (`connected`).
            let waiting = WaitingThread {
                channel,
                message,
                exact,
                view,
            };
            self.waiting_threads.insert(id, waiting);
            return;
        };
        let Ok(viewer) = PublicKey::from_hex(&self.identity.pubkey_hex()) else {
            return;
        };
        let readable = self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.can_access(&channel));
        let seed = readable
            .then(|| {
                self.timelines
                    .get(&channel)
                    .and_then(|timeline| timeline.find(&message))
                    .or_else(|| {
                        self.threads
                            .values()
                            .find_map(|thread| thread.state.event(&message).cloned())
                    })
            })
            .flatten();
        let config = ThreadConfig {
            channel: channel.clone(),
            message_id: message,
            exact,
            relay_author: relay,
            signing_authority: Some(relay),
            viewer,
            authority_host: thread_authority(self.origin.websocket()).ok(),
        };
        let mut state = ThreadState::new(config, self.clock.clone(), seed.as_ref());
        state.set_admins(&self.channel_admins(&channel));
        let retained: Vec<Verified> = self
            .timelines
            .get(&channel)
            .map(|timeline| timeline.events())
            .unwrap_or_default();
        state.receive(&retained);
        state.set_local(&self.local_operations());
        let first = state.refresh();
        let mut thread = OpenThread {
            state,
            channel,
            view,
            retarget: None,
        };
        thread.publish();
        self.threads.insert(id, thread);
        if let Some(read) = first {
            self.thread_read(id, read);
        }
    }

    /// Opens the thread views that waited for the relay's identity.
    pub(super) fn open_waiting_threads(&mut self) {
        for (id, waiting) in std::mem::take(&mut self.waiting_threads) {
            if !waiting.view.is_closed() {
                let WaitingThread {
                    channel,
                    message,
                    exact,
                    view,
                } = waiting;
                self.open_thread(id, channel, message, exact, view);
            }
        }
    }

    pub(super) fn thread_read(&self, id: u64, read: ThreadRead) {
        let Some(thread) = self.threads.get(&id) else {
            return;
        };
        let channel = thread.channel.clone();
        let reader = self.reader.clone();
        let commands = self.commands.clone();
        self.runtime.spawn(async move {
            let result = reader.read(read.filters, read.options).await;
            let _ = commands.send(Message::ThreadRead {
                id,
                token: read.token,
                channel,
                result,
            });
        });
    }

    pub(super) fn thread_result(
        &mut self,
        id: u64,
        token: u64,
        channel: &str,
        result: Result<Events, ReadError>,
    ) {
        // A read that finished after access was lost must not reveal anything.
        let readable = self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.can_access(channel));
        let result = if readable {
            result
        } else {
            Err(ReadError {
                kind: ReadErrorKind::Unavailable,
                message: "Conversation is unavailable".into(),
                status: None,
                retry_after: None,
            })
        };
        let Some(thread) = self.threads.get_mut(&id) else {
            return;
        };
        let mut completion = thread.state.complete(token, result);
        thread.settle(completion.retarget);
        if completion.denied {
            if let Some(discovery) = &mut self.discovery {
                discovery.deny(channel);
            }
            self.purge_denied();
            self.publish();
        }
        let visible = if completion.admit.is_empty() {
            Vec::new()
        } else {
            self.accept(&completion.admit)
        };
        // An exact target's fold goes on with what the session kept
        // (`acceptTarget`).
        if completion.admission {
            let Some(thread) = self.threads.get_mut(&id) else {
                return;
            };
            completion = thread.state.admitted(&visible);
            thread.settle(completion.retarget);
        }
        if let Some(next) = completion.next {
            self.thread_read(id, next);
        }
    }

    pub(super) fn thread_retarget(
        &mut self,
        id: u64,
        message: EventId,
        reply: oneshot::Sender<Retarget>,
    ) {
        let Some(thread) = self.threads.get_mut(&id) else {
            let _ = reply.send(Retarget::Cancelled);
            return;
        };
        let readable = self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.can_access(&thread.channel));
        if !readable {
            let _ = reply.send(Retarget::Cancelled);
            return;
        }
        match thread.state.retarget(message) {
            Err(outcome) => {
                let _ = reply.send(outcome);
            }
            Ok(read) => {
                if let Some(earlier) = thread.retarget.replace(reply) {
                    let _ = earlier.send(Retarget::Cancelled);
                }
                thread.publish();
                self.thread_read(id, read);
            }
        }
    }

    pub(super) fn thread_more(&mut self, id: u64, refresh: bool) {
        let Some(thread) = self.threads.get_mut(&id) else {
            return;
        };
        let read = if refresh {
            thread.state.refresh()
        } else {
            thread.state.load_more()
        };
        thread.publish();
        if let Some(read) = read {
            self.thread_read(id, read);
        }
    }

    pub(super) fn threads_receive(&mut self, events: &[Verified]) {
        for thread in self.threads.values_mut() {
            if thread.state.receive(events) {
                thread.publish();
            }
        }
    }

    pub(super) fn threads_local(&mut self, local: &[(Verified, LocalDelivery)]) {
        for thread in self.threads.values_mut() {
            thread.state.set_local(local);
            thread.publish();
        }
    }

    /// The channel's live route is back: repair what each view retained.
    pub(super) fn threads_established(&mut self, channels: &[String]) {
        let reads: Vec<(u64, ThreadRead)> = self
            .threads
            .iter_mut()
            .filter(|(_, thread)| channels.contains(&thread.channel))
            .filter_map(|(id, thread)| thread.state.refresh().map(|read| (*id, read)))
            .collect();
        for (id, read) in reads {
            self.thread_read(id, read);
        }
    }

    /// Revokes views of channels the viewer can no longer read.
    pub(super) fn threads_purge(&mut self) {
        let Some(discovery) = &self.discovery else {
            return;
        };
        for thread in self.threads.values_mut() {
            if !discovery.can_access(&thread.channel) {
                thread.state.revoke();
                thread.publish();
            }
        }
    }
}
