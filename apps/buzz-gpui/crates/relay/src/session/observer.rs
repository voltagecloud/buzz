//! The `observer` live route (relay.md §3.3): kinds 24200 and 44200
//! addressed to the viewer. Two independent taps share it: the
//! archive's capture and the Agent Activity feed that shows the viewer's
//! agents at work. The route asks for the union of what they want.

use super::{Actor, Message, Session};
use crate::event::{Verified, kinds};
use crate::live::{RouteKey, RouteStatus};
use nostr::prelude::Event;
use std::sync::Arc;
use tokio::sync::mpsc;

/// One listener on the route and the kinds it takes.
struct Tap<T> {
    kinds: Vec<u16>,
    sender: mpsc::UnboundedSender<T>,
}

/// What the session captures and where it sends it.
#[derive(Default)]
pub(super) struct Observer {
    capture: Option<Tap<Arc<Event>>>,
    activity: Option<Tap<ActivityFeed>>,
    /// The feed state last sent to the activity tap.
    status: Option<FeedStatus>,
}

/// Whether the live activity feed is receiving (`activity.ts` `status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedStatus {
    /// Connecting, or the route is still being set up.
    Connecting,
    /// The route reached EOSE on a connected socket.
    Listening,
    /// The socket dropped or the route failed; quiet means unknown.
    Interrupted,
}

/// What the activity tap receives.
#[derive(Clone, Debug)]
pub enum ActivityFeed {
    /// A signature-checked agent activity envelope, still encrypted.
    Frame(Arc<Event>),
    Status(FeedStatus),
}

impl Session {
    /// Receives the viewer's agent `kinds` (24200 activity, 44200 metrics)
    /// live from now on, replacing an earlier capture. Empty `kinds`
    /// stops it. Envelopes arrive signature-checked but not decrypted.
    pub fn observe(&self, kinds: Vec<u16>) -> mpsc::UnboundedReceiver<Arc<Event>> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            let observer = actor.observer.get_or_insert_default();
            observer.capture = (!kinds.is_empty()).then_some(Tap { kinds, sender });
            actor.observe_live();
        })));
        receiver
    }

    /// The live agent activity feed (kind 24200 addressed to the viewer)
    /// and its state, replacing an earlier feed. Dropping the receiver
    /// stops it once the next frame or state change finds nobody there.
    pub fn observe_activity(&self) -> mpsc::UnboundedReceiver<ActivityFeed> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            let observer = actor.observer.get_or_insert_default();
            observer.activity = Some(Tap {
                kinds: vec![kinds::AGENT_ACTIVITY],
                sender,
            });
            observer.status = None;
            actor.observe_live();
            actor.observer_status();
        })));
        receiver
    }
}

impl Actor {
    /// Applies the taps to the live connection, also after a reconnect.
    pub(super) fn observe_live(&mut self) {
        let mut kinds: Vec<u16> = self
            .observer
            .iter()
            .flat_map(|observer| {
                let capture = observer.capture.iter().flat_map(|tap| tap.kinds.clone());
                let activity = observer.activity.iter().flat_map(|tap| tap.kinds.clone());
                capture.chain(activity).collect::<Vec<_>>()
            })
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        if let Some(live) = &mut self.live {
            live.watch_observer(kinds);
        }
    }

    pub(super) fn observed(&mut self, events: Vec<Verified>) {
        let Some(observer) = &mut self.observer else {
            return;
        };
        let mut gone = false;
        for event in events {
            let kind = event.kind_u16();
            if let Some(tap) = &observer.capture
                && tap.kinds.contains(&kind)
                && tap.sender.send(Arc::new(event.event().clone())).is_err()
            {
                // Nobody is listening any more.
                observer.capture = None;
                gone = true;
            }
            if let Some(tap) = &observer.activity
                && tap.kinds.contains(&kind)
                && tap
                    .sender
                    .send(ActivityFeed::Frame(Arc::new(event.event().clone())))
                    .is_err()
            {
                observer.activity = None;
                gone = true;
            }
        }
        if gone {
            self.observe_live();
        }
    }

    /// Tells the activity tap when the feed's state changes: listening
    /// only while the socket is up and the route is live.
    pub(super) fn observer_status(&mut self) {
        let route = self.live.as_ref().map(|live| {
            let status = live
                .statuses()
                .into_iter()
                .find(|(key, _)| *key == RouteKey::Observer)
                .map(|(_, status)| status);
            (live.is_connected(), status)
        });
        let Some(observer) = &mut self.observer else {
            return;
        };
        let was_up = matches!(
            observer.status,
            Some(FeedStatus::Listening | FeedStatus::Interrupted)
        );
        let next = match route {
            Some((true, Some(RouteStatus::Live))) => FeedStatus::Listening,
            Some((_, Some(RouteStatus::Error(_)))) => FeedStatus::Interrupted,
            // Up once, now down: whatever was working is unknown.
            Some((false, _)) | None if was_up => FeedStatus::Interrupted,
            _ => FeedStatus::Connecting,
        };
        let Some(tap) = &observer.activity else {
            return;
        };
        if observer.status == Some(next) {
            return;
        }
        observer.status = Some(next);
        if tap.sender.send(ActivityFeed::Status(next)).is_err() {
            observer.activity = None;
            self.observe_live();
        }
    }
}
