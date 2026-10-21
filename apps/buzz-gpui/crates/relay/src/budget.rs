//! Bounded in-memory caches (`budget.ts`).

use std::collections::{BTreeMap, HashMap};

/// A map with a count and byte budget that evicts its least recently set
/// (or got) entries first (`ByteLru`). `peek` never changes the order.
#[derive(Clone, Debug)]
pub struct ByteLru<V> {
    items: HashMap<String, Entry<V>>,
    order: BTreeMap<u64, String>,
    next: u64,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

#[derive(Clone, Debug)]
struct Entry<V> {
    seq: u64,
    bytes: usize,
    value: V,
}

impl<V> ByteLru<V> {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            items: HashMap::new(),
            order: BTreeMap::new(),
            next: 0,
            bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    /// The value, now the most recently used.
    pub fn get(&mut self, key: &str) -> Option<&V> {
        let entry = self.items.get_mut(key)?;
        self.order.remove(&entry.seq);
        entry.seq = self.next;
        self.order.insert(self.next, key.to_string());
        self.next += 1;
        Some(&entry.value)
    }

    pub fn peek(&self, key: &str) -> Option<&V> {
        self.items.get(key).map(|entry| &entry.value)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.items.contains_key(key)
    }

    /// Inserts as the newest entry and evicts the oldest until both budgets
    /// hold. An entry larger than the whole byte budget is not kept.
    pub fn set(&mut self, key: String, value: V, bytes: usize) -> bool {
        self.remove(&key);
        if bytes > self.max_bytes || self.max_entries == 0 {
            return false;
        }
        self.order.insert(self.next, key.clone());
        self.items.insert(
            key,
            Entry {
                seq: self.next,
                bytes,
                value,
            },
        );
        self.next += 1;
        self.bytes += bytes;
        while self.items.len() > self.max_entries || self.bytes > self.max_bytes {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some(entry) = self.items.remove(&oldest) {
                self.bytes -= entry.bytes;
            }
        }
        true
    }

    pub fn remove(&mut self, key: &str) -> Option<V> {
        let entry = self.items.remove(key)?;
        self.order.remove(&entry.seq);
        self.bytes -= entry.bytes;
        Some(entry.value)
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.order.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Oldest first.
    pub fn keys(&self) -> Vec<String> {
        self.order.values().cloned().collect()
    }

    /// Oldest first.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.order.values().map(|key| (key, &self.items[key].value))
    }

    /// Changes a value in place without touching its order or size.
    pub fn update(&mut self, key: &str, change: impl FnOnce(&mut V)) {
        if let Some(entry) = self.items.get_mut(key) {
            change(&mut entry.value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_the_least_recent_by_count_and_bytes() {
        let mut lru = ByteLru::new(2, 10);
        lru.set("a".into(), 1, 4);
        lru.set("b".into(), 2, 4);
        assert_eq!(lru.get("a"), Some(&1), "a is now the newest");
        lru.set("c".into(), 3, 4);
        assert_eq!(lru.keys(), ["a", "c"]);
        // Over the bytes: the oldest go until it fits.
        lru.set("d".into(), 4, 8);
        assert_eq!(lru.keys(), ["d"]);
        assert_eq!(lru.bytes(), 8);
        // Larger than the whole budget: not kept, and the old value is gone.
        assert!(!lru.set("d".into(), 5, 11));
        assert!(lru.is_empty());
        lru.set("e".into(), 6, 1);
        assert_eq!(lru.peek("e"), Some(&6));
        assert_eq!(lru.remove("e"), Some(6));
        assert_eq!(lru.bytes(), 0);
    }
}
