//! Recent banners, by the id the notification service gave each: where a
//! click goes and the message each is about. Notification services keep
//! banners until they are dismissed, so only the newest are remembered.

use super::Key;
use crate::ui::navigation::Target;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// How many recent banners are remembered.
pub const REMEMBERED: usize = 64;

pub struct Shown<Id> {
    pub targets: HashMap<Id, Target>,
    order: VecDeque<Id>,
    pub keys: HashMap<Id, Key>,
}

impl<Id> Default for Shown<Id> {
    fn default() -> Self {
        Self {
            targets: HashMap::new(),
            order: VecDeque::new(),
            keys: HashMap::new(),
        }
    }
}

impl<Id: Clone + Eq + Hash> Shown<Id> {
    /// Remembers a banner; returns the oldest one when it is forgotten to
    /// make room.
    pub fn insert(&mut self, id: Id, target: Target, key: Option<Key>) -> Option<Id> {
        let evicted = (self.order.len() >= REMEMBERED)
            .then(|| self.order.pop_front())
            .flatten();
        if let Some(oldest) = &evicted {
            self.targets.remove(oldest);
            self.keys.remove(oldest);
        }
        self.order.push_back(id.clone());
        if let Some(key) = key {
            self.keys.insert(id.clone(), key);
        }
        self.targets.insert(id, target);
        evicted
    }

    /// Forgets a banner, returning where its click went.
    pub fn forget(&mut self, id: &Id) -> Option<Target> {
        self.order.retain(|shown| shown != id);
        self.keys.remove(id);
        self.targets.remove(id)
    }

    /// The banners about `key`'s message, no longer tied to it.
    pub fn withdraw(&mut self, key: &Key) -> Vec<Id> {
        let ids: Vec<Id> = self
            .keys
            .iter()
            .filter(|(_, shown)| *shown == key)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            self.keys.remove(id);
        }
        ids
    }
}
