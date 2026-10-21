//! Message banners taken back once their message is read in Buzz (a port
//! addition; buzz-app leaves them to the notification center).
//!
//! A banner is read when the focused window opens its conversation (the
//! channel for a top-level message, the thread for a reply), or when its
//! message stops being unread: read in view, or read on another device.
//! Read state is followed with one [`UnreadWatch`] per channel that has
//! banners up. Only the most recent banners are remembered.

use super::{Conversation, Key};
use crate::community::Community;
use crate::runtime::Runtime;
use buzz_relay_client::{EventId, UnreadWatch};
use futures::StreamExt;
use gpui_kit::{App, Entity, Global, Task};
use std::collections::{HashMap, HashSet, VecDeque};

/// Banners remembered for withdrawal, as many as the platforms remember
/// for clicks.
const REMEMBERED: usize = 64;

/// A banner that is up, as far as the app knows.
struct Posted {
    key: Key,
    /// Its message was seen unread, so reading it is a change.
    seen_unread: bool,
}

/// The banners that are up, oldest first.
#[derive(Default)]
struct Live {
    posted: VecDeque<Posted>,
}

impl Live {
    /// Remembers `key`, and returns the banner forgotten to make room.
    fn post(&mut self, key: Key) -> Option<Key> {
        self.posted.retain(|posted| posted.key != key);
        let forgotten = (self.posted.len() >= REMEMBERED)
            .then(|| self.posted.pop_front())
            .flatten()
            .map(|posted| posted.key);
        self.posted.push_back(Posted {
            key,
            seen_unread: false,
        });
        forgotten
    }

    /// Takes the banners `conversation` shows.
    fn opened(&mut self, conversation: &Conversation) -> Vec<Key> {
        self.take(|posted| conversation.shows(&posted.key))
    }

    /// Takes the banners in `channel` whose message was unread and no
    /// longer is. A report may predate a banner, so a message never seen
    /// unread is left alone.
    fn reported(&mut self, origin: &str, channel: &str, unread: &HashSet<EventId>) -> Vec<Key> {
        for posted in &mut self.posted {
            if posted.key.in_channel(origin, channel) && unread.contains(&posted.key.message) {
                posted.seen_unread = true;
            }
        }
        self.take(|posted| {
            posted.key.in_channel(origin, channel)
                && posted.seen_unread
                && !unread.contains(&posted.key.message)
        })
    }

    fn take(&mut self, mut read: impl FnMut(&Posted) -> bool) -> Vec<Key> {
        let mut taken = Vec::new();
        self.posted.retain(|posted| {
            let keep = !read(posted);
            if !keep {
                taken.push(posted.key.clone());
            }
            keep
        });
        taken
    }

    /// The messages with banners up in `channel`.
    fn messages(&self, origin: &str, channel: &str) -> Vec<EventId> {
        self.posted
            .iter()
            .filter(|posted| posted.key.in_channel(origin, channel))
            .map(|posted| posted.key.message)
            .collect()
    }
}

/// One channel's read-state watch.
struct Watched {
    watch: UnreadWatch,
    _task: Task<()>,
}

#[derive(Default)]
struct Withdrawals {
    live: Live,
    /// By community origin and channel.
    watches: HashMap<(String, String), Watched>,
}

impl Global for Withdrawals {}

/// The focused window opened `conversation`: its banners are read.
pub fn opened(conversation: &Conversation, cx: &mut App) {
    let read = cx.default_global::<Withdrawals>().live.opened(conversation);
    take_back(read, cx);
}

/// Remembers a banner just handed to the platform, and follows its
/// message's read state in `community`.
pub fn posted(key: Key, community: &Entity<Community>, cx: &mut App) {
    let channel = (key.origin.clone(), key.channel.clone());
    let forgotten = cx.default_global::<Withdrawals>().live.post(key);
    if !cx.global::<Withdrawals>().watches.contains_key(&channel) {
        let watch = community.read(cx).session.watch_unread(&channel.1);
        let task = follow(&watch, channel.clone(), cx);
        cx.global_mut::<Withdrawals>()
            .watches
            .insert(channel.clone(), Watched { watch, _task: task });
    }
    let mut changed = vec![channel];
    changed.extend(forgotten.map(|key| (key.origin, key.channel)));
    refresh(changed, cx);
}

/// Reports each change of the watch's unread rows until the watch ends.
fn follow(watch: &UnreadWatch, channel: (String, String), cx: &mut App) -> Task<()> {
    let mut receiver = watch.snapshot.clone();
    let (sender, mut values) = futures::channel::mpsc::unbounded();
    Runtime::handle(cx).spawn(async move {
        while receiver.changed().await.is_ok() {
            let value = receiver.borrow_and_update().clone();
            if sender.unbounded_send(value).is_err() {
                return;
            }
        }
    });
    cx.spawn(async move |cx| {
        while let Some(mut detail) = values.next().await {
            // Only the newest pending report matters.
            while let Ok(newer) = values.try_recv() {
                detail = newer;
            }
            cx.update(|cx| {
                let (origin, id) = &channel;
                let read =
                    cx.default_global::<Withdrawals>()
                        .live
                        .reported(origin, id, &detail.unread);
                take_back(read, cx);
            });
        }
    })
}

/// Withdraws `read` from the notification center and stops watching
/// channels with no banners left.
fn take_back(read: Vec<Key>, cx: &mut App) {
    if read.is_empty() {
        return;
    }
    let channels = read
        .iter()
        .map(|key| (key.origin.clone(), key.channel.clone()))
        .collect();
    for key in &read {
        super::withdraw(key, cx);
    }
    refresh(channels, cx);
}

/// Points each channel's watch at its banners' messages, or ends it.
fn refresh(channels: Vec<(String, String)>, cx: &mut App) {
    let this = cx.default_global::<Withdrawals>();
    for channel in channels {
        let messages = this.live.messages(&channel.0, &channel.1);
        if messages.is_empty() {
            this.watches.remove(&channel);
        } else if let Some(watched) = this.watches.get(&channel) {
            watched.watch.set(Vec::new(), messages);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> EventId {
        EventId::from_byte_array([byte; 32])
    }

    fn key(channel: &str, message: u8, thread: Option<u8>) -> Key {
        Key {
            origin: "ws://relay".into(),
            channel: channel.into(),
            message: id(message),
            thread: thread.map(id),
        }
    }

    fn open(channel: &str, thread: Option<u8>) -> Conversation {
        Conversation {
            community: "ws://relay".into(),
            channel: channel.into(),
            thread: thread.map(id),
        }
    }

    #[test]
    fn opening_a_conversation_reads_its_banners() {
        let mut live = Live::default();
        live.post(key("general", 1, None));
        live.post(key("general", 2, Some(9)));
        live.post(key("random", 3, None));
        // The channel reads its top-level message, not the thread reply.
        assert_eq!(
            live.opened(&open("general", None)),
            [key("general", 1, None)]
        );
        assert_eq!(
            live.opened(&open("general", Some(9))),
            [key("general", 2, Some(9))]
        );
        assert_eq!(live.messages("ws://relay", "random"), [id(3)]);
        assert!(live.messages("ws://relay", "general").is_empty());
        // Another community's channel of the same id is not open.
        let mut elsewhere = open("random", None);
        elsewhere.community = "ws://other".into();
        assert!(live.opened(&elsewhere).is_empty());
    }

    #[test]
    fn a_message_read_after_being_unread_is_withdrawn() {
        let mut live = Live::default();
        live.post(key("general", 1, None));
        live.post(key("general", 2, None));
        // A report from before the banners names neither: nothing is read.
        assert!(
            live.reported("ws://relay", "general", &HashSet::new())
                .is_empty()
        );
        let both = HashSet::from([id(1), id(2)]);
        assert!(live.reported("ws://relay", "general", &both).is_empty());
        // Message 1 was read here or on another device.
        let read = live.reported("ws://relay", "general", &HashSet::from([id(2)]));
        assert_eq!(read, [key("general", 1, None)]);
        assert_eq!(live.messages("ws://relay", "general"), [id(2)]);
        // Other channels' reports leave it alone.
        assert!(
            live.reported("ws://relay", "random", &HashSet::new())
                .is_empty()
        );
    }

    #[test]
    fn only_recent_banners_are_remembered() {
        let mut live = Live::default();
        for n in 0..REMEMBERED as u8 {
            assert_eq!(live.post(key("general", n, None)), None);
        }
        assert_eq!(
            live.post(key("general", 200, None)),
            Some(key("general", 0, None))
        );
        assert_eq!(live.posted.len(), REMEMBERED);
        // A banner shown again is remembered once.
        assert_eq!(live.post(key("general", 200, None)), None);
        assert_eq!(live.posted.len(), REMEMBERED);
    }
}
