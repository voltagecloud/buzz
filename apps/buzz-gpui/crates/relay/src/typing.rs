//! Who is typing (`typing.ts`, spec §5h.6).
//!
//! Agents publish kind 20002 pulses about every 3 s while they work; this
//! client only receives them. A pulse shows its author as typing in a
//! channel, or in one thread, for [`ACTIVITY_LIFETIME_MS`]. A real message
//! from the same author in the same place hides the indicator at once and
//! ignores further pulses for [`POST_MESSAGE_QUIET_MS`], so a late pulse
//! cannot bring it back right after the reply landed. Nothing here enters
//! history, unread or any cache.

use crate::event::{Verified, kinds};
use crate::unread::thread_reference;
use std::collections::HashMap;
use std::sync::Arc;

pub const ACTIVITY_LIFETIME_MS: u64 = 8_000;
pub const POST_MESSAGE_QUIET_MS: u64 = 2_000;
const MAX_RECORDS: usize = 1024;

/// Typing people per conversation, in the order they started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypingSnapshot {
    /// Channel id → people typing at the top level.
    pub channels: Arc<HashMap<String, Vec<String>>>,
    /// Thread root id → people typing in that thread.
    pub threads: Arc<HashMap<String, Vec<String>>>,
    /// Channel id → people typing anywhere in it, threads included, each
    /// once (the sidebar's DM typing badge).
    pub anywhere: Arc<HashMap<String, Vec<String>>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    channel: String,
    root: Option<String>,
    author: String,
}

struct Record {
    /// Admission order, for a stable display order.
    order: u64,
    last_activity: Option<u64>,
    last_message: Option<u64>,
    visible_until: u64,
    quiet_until: u64,
}

impl Record {
    fn retain_until(&self) -> u64 {
        let latest = self.last_activity.max(self.last_message).unwrap_or(0);
        (latest + ACTIVITY_LIFETIME_MS).max(self.quiet_until)
    }
}

pub struct Typing {
    viewer: String,
    records: HashMap<Key, Record>,
    next_order: u64,
    snapshot: TypingSnapshot,
    /// When the snapshot was last computed; visibility ends after it are
    /// still to come.
    published_at: u64,
}

fn valid_channel(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A pulse's `e` tags must name one unambiguous thread scope.
fn canonical_scope(event: &Verified) -> bool {
    let refs: Vec<&[String]> = event.tags_named("e").collect();
    if refs.is_empty() {
        return true;
    }
    let marker = |tag: &[String]| {
        tag.get(3)
            .map(String::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    refs.len() <= 2
        && refs.iter().all(|tag| {
            tag.get(1).is_some_and(|id| hex64(id))
                && matches!(marker(tag).as_str(), "root" | "reply")
        })
        && refs.iter().filter(|tag| marker(tag) == "reply").count() == 1
        && refs.iter().filter(|tag| marker(tag) == "root").count() <= 1
}

impl Typing {
    pub fn new(viewer: impl Into<String>) -> Self {
        Self {
            viewer: viewer.into(),
            records: HashMap::new(),
            next_order: 0,
            snapshot: TypingSnapshot::default(),
            published_at: 0,
        }
    }

    pub fn snapshot(&self) -> &TypingSnapshot {
        &self.snapshot
    }

    /// Takes verified live events: kind 20002 pulses and the messages that
    /// end them. `can_access` is the channel visibility check. Returns
    /// whether the snapshot changed.
    pub fn accept(
        &mut self,
        events: &[Verified],
        can_access: impl Fn(&str) -> bool,
        now: u64,
    ) -> bool {
        self.expire_silence(now);
        // Completion wins even when the pulse came first in the batch.
        for event in events {
            if matches!(
                event.kind_u16(),
                kinds::CHAT | kinds::AGENT_MESSAGE | kinds::DIFF
            ) {
                self.receive(event, false, &can_access, now);
            }
        }
        for event in events {
            if event.kind_u16() == kinds::TYPING {
                self.receive(event, true, &can_access, now);
            }
        }
        self.publish(now)
    }

    /// Hides pulses whose lifetime ended. Returns whether the snapshot
    /// changed.
    pub fn tick(&mut self, now: u64) -> bool {
        self.expire_silence(now);
        self.publish(now)
    }

    /// Forgets everything, e.g. when live traffic stops.
    pub fn clear(&mut self) -> bool {
        self.records.clear();
        self.publish(0)
    }

    /// When the snapshot next changes on its own.
    pub fn deadline(&self) -> Option<u64> {
        self.records
            .values()
            .flat_map(|record| {
                let visible =
                    (record.visible_until > self.published_at).then_some(record.visible_until);
                [Some(record.retain_until()), visible]
            })
            .flatten()
            .min()
    }

    fn expire_silence(&mut self, now: u64) {
        self.records.retain(|_, record| record.retain_until() > now);
    }

    fn receive(
        &mut self,
        event: &Verified,
        typing: bool,
        can_access: &impl Fn(&str) -> bool,
        now: u64,
    ) {
        let author = event.pubkey_hex();
        if author == self.viewer {
            return;
        }
        let at = event.created_at_secs().saturating_mul(1000);
        if at > now || at + ACTIVITY_LIFETIME_MS <= now {
            return;
        }
        let mut channels = event.tags_named("h");
        let channel = match (channels.next(), channels.next()) {
            (Some(tag), None) => tag.get(1).cloned().unwrap_or_default(),
            _ => return,
        };
        if !valid_channel(&channel) || !can_access(&channel) {
            return;
        }
        if typing && !canonical_scope(event) {
            return;
        }
        let key = Key {
            channel,
            root: thread_reference(event).map(|reference| reference.root_id),
            author,
        };
        self.receive_at(key, at, typing, now);
    }

    fn receive_at(&mut self, key: Key, at: u64, typing: bool, now: u64) {
        if at > now || at + ACTIVITY_LIFETIME_MS <= now {
            return;
        }
        if !self.records.contains_key(&key) {
            if self.records.len() >= MAX_RECORDS {
                return;
            }
            self.next_order += 1;
            self.records.insert(
                key.clone(),
                Record {
                    order: self.next_order,
                    last_activity: None,
                    last_message: None,
                    visible_until: 0,
                    quiet_until: 0,
                },
            );
        }
        let record = self.records.get_mut(&key).expect("inserted");
        if typing {
            if record.last_activity.is_some_and(|last| at <= last)
                || record.last_message.is_some_and(|last| at <= last)
            {
                return;
            }
            // Pulses during the quiet period are remembered, so they cannot
            // be replayed once it ends.
            record.last_activity = Some(at);
            if now >= record.quiet_until {
                record.visible_until = at + ACTIVITY_LIFETIME_MS;
            }
        } else {
            if record.last_message.is_some_and(|last| at <= last) {
                return;
            }
            record.last_message = Some(at);
            if record.last_activity.is_some_and(|last| at < last) {
                return;
            }
            record.visible_until = 0;
            record.quiet_until = now + POST_MESSAGE_QUIET_MS;
        }
    }

    /// Rebuilds the snapshot, keeping each map's `Arc` when it is unchanged.
    fn publish(&mut self, now: u64) -> bool {
        self.published_at = now;
        let mut visible: Vec<(&Key, u64)> = self
            .records
            .iter()
            .filter(|(_, record)| record.visible_until > now)
            .map(|(key, record)| (key, record.order))
            .collect();
        visible.sort_by_key(|(_, order)| *order);
        let mut channels: HashMap<String, Vec<String>> = HashMap::new();
        let mut threads: HashMap<String, Vec<String>> = HashMap::new();
        let mut anywhere: HashMap<String, Vec<String>> = HashMap::new();
        for (key, _) in visible {
            let people = anywhere.entry(key.channel.clone()).or_default();
            if !people.contains(&key.author) {
                people.push(key.author.clone());
            }
            let list = match &key.root {
                Some(root) => threads.entry(root.clone()).or_default(),
                None => channels.entry(key.channel.clone()).or_default(),
            };
            list.push(key.author.clone());
        }
        let mut changed = false;
        if *self.snapshot.channels != channels {
            self.snapshot.channels = Arc::new(channels);
            changed = true;
        }
        if *self.snapshot.threads != threads {
            self.snapshot.threads = Arc::new(threads);
            changed = true;
        }
        if *self.snapshot.anywhere != anywhere {
            self.snapshot.anywhere = Arc::new(anywhere);
            changed = true;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Verifier;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};

    const NOW: u64 = 1_760_000_000_000;

    fn event(keys: &Keys, kind: u16, at_ms: u64, tags: Vec<Vec<&str>>) -> Verified {
        let tags = tags
            .into_iter()
            .map(|tag| Tag::custom(tag[0], tag[1..].iter().copied()))
            .collect::<Vec<_>>();
        let event = EventBuilder::new(Kind::from_u16(kind), "")
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(at_ms / 1000))
            .finalize(keys)
            .unwrap();
        Verifier::default().verify(event).unwrap()
    }

    fn pulse(keys: &Keys, at_ms: u64, channel: &str) -> Verified {
        event(keys, kinds::TYPING, at_ms, vec![vec!["h", channel]])
    }

    fn hex(keys: &Keys) -> String {
        keys.public_key().to_hex()
    }

    fn any(_: &str) -> bool {
        true
    }

    #[test]
    fn pulses_show_until_their_lifetime_ends() {
        let (viewer, agent) = (Keys::generate(), Keys::generate());
        let mut typing = Typing::new(hex(&viewer));
        assert!(typing.accept(&[pulse(&agent, NOW - 1_000, "room")], any, NOW));
        let channels = typing.snapshot().channels.clone();
        assert_eq!(channels["room"], vec![hex(&agent)]);
        // Nothing changed: the same Arc.
        assert!(!typing.tick(NOW + 1_000));
        assert!(Arc::ptr_eq(&channels, &typing.snapshot().channels));
        assert_eq!(typing.deadline(), Some(NOW - 1_000 + ACTIVITY_LIFETIME_MS));
        assert!(typing.tick(NOW + 7_000));
        assert!(typing.snapshot().channels.is_empty());
        assert_eq!(typing.deadline(), None);
    }

    #[test]
    fn ignores_own_stale_future_and_inaccessible_pulses() {
        let (viewer, agent) = (Keys::generate(), Keys::generate());
        let mut typing = Typing::new(hex(&viewer));
        let events = [
            pulse(&viewer, NOW, "room"),
            pulse(&agent, NOW - ACTIVITY_LIFETIME_MS, "room"),
            pulse(&agent, NOW + 1_000, "room"),
            pulse(&agent, NOW, "bad channel!"),
            event(&agent, kinds::TYPING, NOW, vec![]),
        ];
        assert!(!typing.accept(&events, any, NOW));
        assert!(!typing.accept(&[pulse(&agent, NOW, "secret")], |id| id != "secret", NOW));
        assert!(typing.snapshot().channels.is_empty());
    }

    #[test]
    fn a_message_hides_the_pulse_and_quiets_followers() {
        let (viewer, agent) = (Keys::generate(), Keys::generate());
        let mut typing = Typing::new(hex(&viewer));
        typing.accept(&[pulse(&agent, NOW - 2_000, "room")], any, NOW);
        let message = event(&agent, kinds::CHAT, NOW - 1_000, vec![vec!["h", "room"]]);
        assert!(typing.accept(&[message], any, NOW));
        assert!(typing.snapshot().channels.is_empty());
        // A pulse during the quiet period stays hidden.
        typing.accept(&[pulse(&agent, NOW + 1_000, "room")], any, NOW + 1_000);
        assert!(typing.snapshot().channels.is_empty());
        // A later one shows again.
        typing.accept(&[pulse(&agent, NOW + 3_000, "room")], any, NOW + 3_000);
        assert_eq!(typing.snapshot().channels["room"].len(), 1);
    }

    #[test]
    fn thread_pulses_are_scoped_to_their_root() {
        let (viewer, agent, other) = (Keys::generate(), Keys::generate(), Keys::generate());
        let root = "a".repeat(64);
        let parent = "b".repeat(64);
        let mut typing = Typing::new(hex(&viewer));
        let in_thread = event(
            &agent,
            kinds::TYPING,
            NOW,
            vec![
                vec!["h", "room"],
                vec!["e", &root, "", "root"],
                vec!["e", &parent, "", "reply"],
            ],
        );
        let ambiguous = event(
            &other,
            kinds::TYPING,
            NOW,
            vec![vec!["h", "room"], vec!["e", &root, "", "root"]],
        );
        typing.accept(
            &[in_thread, ambiguous, pulse(&other, NOW, "room")],
            any,
            NOW,
        );
        let snapshot = typing.snapshot();
        assert_eq!(snapshot.threads[&root], vec![hex(&agent)]);
        assert_eq!(snapshot.channels["room"], vec![hex(&other)]);
    }

    #[test]
    fn keeps_arrival_order() {
        let viewer = Keys::generate();
        let people: Vec<Keys> = (0..3).map(|_| Keys::generate()).collect();
        let mut typing = Typing::new(hex(&viewer));
        for (n, person) in people.iter().enumerate() {
            typing.accept(&[pulse(person, NOW, "room")], any, NOW + n as u64);
        }
        let order: Vec<String> = people.iter().map(hex).collect();
        assert_eq!(typing.snapshot().channels["room"], order);
        assert!(typing.clear());
        assert!(typing.snapshot().channels.is_empty());
    }
}
