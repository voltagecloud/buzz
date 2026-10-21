//! Channel admins for the fold's moderator deletes (spec §9.3 #7).
//!
//! The relay soft-deletes the target of an admin's 9005, but the fold
//! applies one only from a signer in
//! [`FoldOptions::channel_admins`](crate::fold::FoldOptions::channel_admins).
//! The session keeps each channel's admin keys from its relay-signed 39001:
//! read when a window opens, and kept current by live 39001s. A change
//! refolds the channel's open window and threads and its unread evidence.
//! Without a set a channel folds as the reference does.

use super::{Actor, Message};
use crate::budget::ByteLru;
use crate::event::{Verified, kinds};
use crate::lifecycle;
use crate::reader::{Priority, ReadOptions};
use nostr::prelude::{EventId, PublicKey};
use std::collections::HashSet;

/// Channels whose admins are kept, and their total size.
const MAX_ADMIN_SETS: usize = 256;
const MAX_ADMIN_BYTES: usize = 1 << 20;

/// One channel's admins, from its newest relay-signed 39001.
pub(super) struct ChannelAdmins {
    created_at: u64,
    id: EventId,
    keys: HashSet<PublicKey>,
}

impl ChannelAdmins {
    /// Replaceable-event order, as [`crate::event::is_newer`].
    fn newer_than(&self, event: &Verified) -> bool {
        let at = event.created_at_secs();
        self.created_at > at || (self.created_at == at && self.id <= event.id)
    }
}

pub(super) fn cache() -> ByteLru<ChannelAdmins> {
    ByteLru::new(MAX_ADMIN_SETS, MAX_ADMIN_BYTES)
}

impl Actor {
    /// The admins of `channel`, or none while its 39001 is unknown.
    pub(super) fn channel_admins(&self, channel: &str) -> HashSet<PublicKey> {
        self.channels
            .admins
            .peek(channel)
            .map(|admins| admins.keys.clone())
            .unwrap_or_default()
    }

    /// Reads the channel's 39001 once per window (and again after an
    /// access change). The answer is admitted like any read, through
    /// `accept`.
    pub(super) fn read_admins(&mut self, channel: &str) {
        let Some(authority) = self.relay.as_ref().map(|relay| relay.authority.clone()) else {
            return;
        };
        let Some(timeline) = self.timelines.get_mut(channel) else {
            return;
        };
        if timeline.admins_read {
            return;
        }
        timeline.admins_read = true;
        let generation = timeline.generation;
        let filter = lifecycle::state_filter(kinds::CHANNEL_ADMINS, &authority, channel);
        let reader = self.reader.clone();
        let commands = self.commands.clone();
        let epoch = self.epoch;
        let access = self.access_version(channel);
        let channel = channel.to_string();
        self.runtime.spawn(async move {
            let options = ReadOptions {
                priority: Priority::Background,
                fresh: false,
            };
            let result = reader.read(vec![filter], options).await;
            let _ = commands.send(Message::With(Box::new(move |actor| {
                if epoch != actor.epoch || access != actor.access_version(&channel) {
                    return;
                }
                match result {
                    Ok(events) => {
                        actor.accept(&events);
                    }
                    // The next ensure of this window tries again.
                    Err(_) => {
                        if let Some(timeline) = actor.timelines.get_mut(&channel)
                            && timeline.generation == generation
                        {
                            timeline.admins_read = false;
                        }
                    }
                }
            })));
        });
    }

    /// Takes the channel admins from visible relay-signed 39001s. Returns
    /// whether the session should publish.
    pub(super) fn admins_accept(&mut self, events: &[Verified]) -> bool {
        let Some(authority) = self.relay.as_ref().map(|relay| relay.authority.clone()) else {
            return false;
        };
        let mut changed = Vec::new();
        for event in events {
            if event.kind_u16() != kinds::CHANNEL_ADMINS {
                continue;
            }
            let Some(channel) = event.tag("d") else {
                continue;
            };
            let Some(keys) = lifecycle::channel_admins(event, channel, &authority) else {
                continue;
            };
            let known = self.channels.admins.peek(channel);
            if known.is_some_and(|admins| admins.newer_than(event)) {
                continue;
            }
            let bytes = keys.len() * 32 + channel.len() + 64;
            let admins = ChannelAdmins {
                created_at: event.created_at_secs(),
                id: event.id,
                keys,
            };
            self.channels.admins.set(channel.to_string(), admins, bytes);
            changed.push(channel.to_string());
        }
        for channel in &changed {
            self.apply_admins(channel);
        }
        !changed.is_empty()
    }

    /// Hands every known admin set to a newly opened unread owner.
    pub(super) fn admins_to_unread(&mut self) {
        let Some(reads) = &mut self.reads else {
            return;
        };
        for (channel, admins) in self.channels.admins.iter() {
            reads.unread.set_channel_admins(channel, &admins.keys);
        }
    }

    /// Refolds what shows `channel` with its current admins.
    fn apply_admins(&mut self, channel: &str) {
        let admins = self.channel_admins(channel);
        if let Some(timeline) = self.timelines.get_mut(channel) {
            timeline.set_admins(&admins);
            timeline.publish();
        }
        self.threads_set_admins(channel, &admins);
        if let Some(reads) = &mut self.reads {
            reads.unread.set_channel_admins(channel, &admins);
        }
    }
}
