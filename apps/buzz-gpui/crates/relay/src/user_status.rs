//! User status (kind 30315, NIP-38; `docs/port/relay.md` §5j): parsing,
//! the event a save publishes, and the statuses of watched people
//! (`user-status.ts` `createUserStatuses`). I/O lives in the session.

use crate::event::{Verified, is_hex64};
use crate::messages::EventTemplate;
use crate::socket::Filter;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub const KIND: u16 = crate::event::kinds::USER_STATUS;
/// Text and emoji limit, in UTF-16 units as the reference counts them.
pub const TEXT_LIMIT: usize = 100;
/// How far past now a save may date its event (clock skew guard).
const MAX_SKEW_SECS: u64 = 300;

/// A status as shown. Empty and expired statuses are `None` instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserStatus {
    pub text: String,
    /// A unicode emoji or `:shortcode:`.
    pub emoji: String,
    pub updated_at: u64,
    pub expires_at: Option<u64>,
}

/// What the user asked for. Empty text and emoji clear the status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusInput {
    pub text: String,
    pub emoji: String,
    pub expires_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StatusError {
    #[error("Keep your status to one short line (100 characters).")]
    TooLong,
    #[error("Choose an expiration in the future.")]
    Expired,
    #[error(
        "Your current status is dated too far in the future. Check your devices’ clocks and try again later."
    )]
    ClockSkew,
}

fn valid_text(text: &str, emoji: &str) -> bool {
    text.encode_utf16().count() <= TEXT_LIMIT
        && emoji.encode_utf16().count() <= TEXT_LIMIT
        && !text.contains(['\r', '\n'])
        && !emoji.contains(['\r', '\n'])
}

/// The read of one author's current status (`#d` `general`).
pub fn filter(author: &str) -> Filter {
    crate::socket::filter(
        json!({"kinds": [KIND], "#d": ["general"], "authors": [author], "limit": 1}),
    )
}

/// The newest valid event's status, or `None` when there is none, it was
/// cleared, or it has expired by `now`.
pub fn current(events: &[Verified], author: &str, now: u64) -> Option<UserStatus> {
    events
        .iter()
        .filter(|event| event.pubkey_hex() == author)
        .filter_map(|event| parse(event).map(|status| (event, status)))
        .max_by(|(a, _), (b, _)| {
            if crate::event::is_newer(a, b) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        })
        .map(|(_, status)| status)
        .filter(|status| !(status.text.is_empty() && status.emoji.is_empty()))
        .filter(|status| status.expires_at.is_none_or(|at| at > now))
}

/// Newest `created_at` among the author's valid status events, cleared or
/// expired ones included: a save must replace it.
pub fn latest_created_at(events: &[Verified], author: &str) -> Option<u64> {
    events
        .iter()
        .filter(|event| event.pubkey_hex() == author && parse(event).is_some())
        .map(Verified::created_at_secs)
        .max()
}

/// The current statuses of watched people and the viewer, by pubkey.
/// Cleared and expired statuses are absent.
pub type Statuses = Arc<HashMap<String, UserStatus>>;

/// Authors per status read (`user-status.ts`).
pub const READ_BATCH: usize = 100;
/// A failed read is retried this many times, after these delays, and then
/// left until something new is watched: statuses are optional decoration,
/// not worth an endless poll.
const RETRY_DELAYS_MS: [u64; 3] = [5_000, 15_000, 60_000];

/// The read of several authors' current statuses.
pub fn batch_filter(authors: &[String]) -> Filter {
    crate::socket::filter(
        json!({"kinds": [KIND], "#d": ["general"], "authors": authors, "limit": authors.len()}),
    )
}

/// A status read the directory asked for. Its result goes back to
/// [`StatusDirectory::finished`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusRead {
    pub generation: u64,
    pub authors: Vec<String>,
}

struct Record {
    created_at: u64,
    id: String,
    status: UserStatus,
}

/// The statuses of the people the UI shows (`user-status.ts:44-360`).
/// Watches are counted, so several views can follow one person. Each
/// person's newest event is kept, cleared and expired ones included, so an
/// older replacement can never come back. People no longer watched are
/// forgotten, except the viewer.
pub struct StatusDirectory {
    viewer: String,
    records: HashMap<String, Record>,
    watched: HashMap<String, usize>,
    loaded: HashSet<String>,
    /// Bumped on reconnect, so reads started before it are dropped.
    generation: u64,
    reading: bool,
    retry_attempt: usize,
    /// When the failed read may run again; `u64::MAX` once out of retries.
    retry_at: Option<u64>,
    snapshot: Statuses,
    /// The soonest expiry among shown statuses, in seconds.
    expiry: Option<u64>,
}

/// The keys a watch of `ids` counts: lowercase hex keys, once each.
pub fn watch_keys(ids: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = ids.iter().filter(|id| is_hex64(id)).cloned().collect();
    keys.sort();
    keys.dedup();
    keys
}

impl StatusDirectory {
    pub fn new(viewer: String) -> Self {
        Self {
            viewer,
            records: HashMap::new(),
            watched: HashMap::new(),
            loaded: HashSet::new(),
            generation: 0,
            reading: false,
            retry_attempt: 0,
            retry_at: None,
            snapshot: Statuses::default(),
            expiry: None,
        }
    }

    pub fn snapshot(&self) -> Statuses {
        self.snapshot.clone()
    }

    /// Follows these people ([`watch_keys`]). Returns the keys counted,
    /// which [`Self::unwatch`] takes back. A new watch lifts a read that
    /// ran out of retries.
    pub fn watch(&mut self, ids: &[String]) -> Vec<String> {
        let unique = watch_keys(ids);
        for id in &unique {
            *self.watched.entry(id.clone()).or_default() += 1;
        }
        if !unique.is_empty() && self.retry_at == Some(u64::MAX) {
            self.retry_at = None;
        }
        unique
    }

    /// Stops one watch of each of these people. Returns whether the
    /// snapshot changed.
    pub fn unwatch(&mut self, ids: &[String], now_ms: u64) -> bool {
        for id in ids {
            let Some(count) = self.watched.get_mut(id) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                self.watched.remove(id);
                // The whole record goes, never only its newest evidence.
                if *id != self.viewer {
                    self.loaded.remove(id);
                    self.records.remove(id);
                }
            }
        }
        if self.watched.is_empty() {
            self.retry_at = None;
            self.retry_attempt = 0;
        }
        self.recompute(now_ms)
    }

    /// Takes status events from reads and live traffic. Returns whether the
    /// snapshot changed.
    pub fn accept(&mut self, events: &[Verified], now_ms: u64) -> bool {
        let mut changed = false;
        for event in events {
            if event.kind_u16() != KIND {
                continue;
            }
            let author = event.pubkey_hex();
            // Live traffic about people off screen does not grow the cache.
            if !self.watched.contains_key(&author)
                && author != self.viewer
                && !self.records.contains_key(&author)
            {
                continue;
            }
            let Some(status) = parse(event) else {
                continue;
            };
            let (created_at, id) = (event.created_at_secs(), event.id_hex());
            if let Some(old) = self.records.get(&author)
                && (old.created_at > created_at || (old.created_at == created_at && old.id <= id))
            {
                continue;
            }
            self.records.insert(
                author,
                Record {
                    created_at,
                    id,
                    status,
                },
            );
            changed = true;
        }
        changed && self.recompute(now_ms)
    }

    /// The next read to run, if one is needed and none is in flight or
    /// waiting to retry.
    pub fn next_read(&mut self, now_ms: u64) -> Option<StatusRead> {
        if self.reading || self.retry_at.is_some_and(|at| at > now_ms) {
            return None;
        }
        let mut missing: Vec<String> = self
            .watched
            .keys()
            .filter(|id| !self.loaded.contains(*id))
            .cloned()
            .collect();
        if missing.is_empty() {
            return None;
        }
        missing.sort();
        missing.truncate(READ_BATCH);
        self.reading = true;
        self.retry_at = None;
        Some(StatusRead {
            generation: self.generation,
            authors: missing,
        })
    }

    /// A read finished. A failure is retried while retries remain. Returns
    /// whether the snapshot changed, or `None` for a read from before a
    /// reconnect, which is dropped.
    pub fn finished(
        &mut self,
        read: &StatusRead,
        result: Result<&[Verified], ()>,
        now_ms: u64,
    ) -> Option<bool> {
        if read.generation != self.generation {
            return None;
        }
        self.reading = false;
        match result {
            Ok(events) => {
                let wanted: HashSet<&String> = read.authors.iter().collect();
                let events: Vec<Verified> = events
                    .iter()
                    .filter(|event| wanted.contains(&event.pubkey_hex()))
                    .cloned()
                    .collect();
                let changed = self.accept(&events, now_ms);
                for id in &read.authors {
                    if self.watched.contains_key(id) {
                        self.loaded.insert(id.clone());
                    }
                }
                self.retry_attempt = 0;
                Some(changed)
            }
            Err(()) => {
                self.retry_at = Some(match RETRY_DELAYS_MS.get(self.retry_attempt) {
                    Some(delay) if !self.watched.is_empty() => {
                        self.retry_attempt += 1;
                        now_ms + delay
                    }
                    _ => u64::MAX,
                });
                Some(false)
            }
        }
    }

    /// The connection came back: everything watched is read again.
    pub fn reconnect(&mut self) {
        self.generation += 1;
        self.reading = false;
        self.retry_attempt = 0;
        self.retry_at = None;
        self.loaded.clear();
    }

    /// Access changed or the cache was cleared (`statuses.clear`): every
    /// status is forgotten and the watched people are read again. Returns
    /// whether the snapshot changed.
    pub fn clear(&mut self, now_ms: u64) -> bool {
        self.reconnect();
        self.records.clear();
        self.recompute(now_ms)
    }

    /// The next wall-clock time, in milliseconds, there is work: a status
    /// expiring ([`Self::tick`]) or a retry falling due
    /// ([`Self::next_read`]).
    pub fn deadline(&self) -> Option<u64> {
        [
            self.expiry.map(|at| at.saturating_mul(1000)),
            self.retry_at.filter(|at| *at != u64::MAX),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Drops expired statuses. Returns whether the snapshot changed.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        self.recompute(now_ms)
    }

    fn recompute(&mut self, now_ms: u64) -> bool {
        let mut expiry: Option<u64> = None;
        let next: HashMap<String, UserStatus> = self
            .records
            .iter()
            .filter(|(_, record)| {
                let status = &record.status;
                !(status.text.is_empty() && status.emoji.is_empty())
                    && status
                        .expires_at
                        .is_none_or(|at| at.saturating_mul(1000) > now_ms)
            })
            .map(|(id, record)| {
                if let Some(at) = record.status.expires_at {
                    expiry = Some(expiry.map_or(at, |soonest| soonest.min(at)));
                }
                (id.clone(), record.status.clone())
            })
            .collect();
        self.expiry = expiry;
        if *self.snapshot == next {
            return false;
        }
        self.snapshot = Arc::new(next);
        true
    }
}

/// Parses a status event (`user-status.ts:17-41`). Events with an `h` tag,
/// or without exactly one `d` tag of `general`, are not statuses.
pub fn parse(event: &Verified) -> Option<UserStatus> {
    let d: Vec<&[String]> = event.tags_named("d").collect();
    if event.kind_u16() != KIND
        || d.len() != 1
        || d[0].get(1).map(String::as_str) != Some("general")
        || event.tags_named("h").next().is_some()
    {
        return None;
    }
    let emoji = event.tag("emoji").unwrap_or_default();
    if !valid_text(&event.content, emoji) {
        return None;
    }
    let expires_at = match event.tag("expiration") {
        Some(value) => Some(value.parse::<u64>().ok()?),
        None => None,
    };
    Some(UserStatus {
        text: event.content.trim().to_string(),
        emoji: emoji.trim().to_string(),
        updated_at: event.created_at_secs(),
        expires_at,
    })
}

/// The event that saves `input` (§5j.1), dated after `previous` (the
/// author's newest status event) so it replaces it.
pub fn template(
    input: &StatusInput,
    previous: Option<u64>,
    now: u64,
) -> Result<EventTemplate, StatusError> {
    let text = input.text.trim();
    let emoji = input.emoji.trim();
    if !valid_text(text, emoji) {
        return Err(StatusError::TooLong);
    }
    let expires_at = if text.is_empty() && emoji.is_empty() {
        None
    } else {
        input.expires_at
    };
    if expires_at.is_some_and(|at| at <= now) {
        return Err(StatusError::Expired);
    }
    let created_at = now.max(previous.map_or(0, |at| at + 1));
    if created_at > now + MAX_SKEW_SECS {
        return Err(StatusError::ClockSkew);
    }
    let mut tags = vec![vec!["d".to_string(), "general".to_string()]];
    if !emoji.is_empty() {
        tags.push(vec!["emoji".into(), emoji.into()]);
    }
    if let Some(at) = expires_at {
        tags.push(vec!["expiration".into(), at.to_string()]);
    }
    Ok(EventTemplate {
        kind: KIND,
        created_at,
        tags,
        content: text.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Verifier;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};

    fn event(keys: &Keys, content: &str, at: u64, tags: &[&[&str]]) -> Verified {
        let tags = tags
            .iter()
            .map(|tag| Tag::parse(tag.iter().copied()).unwrap())
            .collect::<Vec<_>>();
        let event = EventBuilder::new(Kind::from_u16(KIND), content)
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(keys)
            .unwrap();
        Verifier::default().verify(event).unwrap()
    }

    #[test]
    fn builds_the_reference_shape() {
        let input = StatusInput {
            text: "  In a meeting ".into(),
            emoji: "📅".into(),
            expires_at: Some(1_760_028_800),
        };
        let template = template(&input, None, 1_760_000_000).unwrap();
        assert_eq!(template.kind, 30315);
        assert_eq!(template.content, "In a meeting");
        assert_eq!(template.created_at, 1_760_000_000);
        assert_eq!(
            template.tags,
            vec![
                vec!["d".to_string(), "general".into()],
                vec!["emoji".into(), "📅".into()],
                vec!["expiration".into(), "1760028800".into()],
            ]
        );
    }

    #[test]
    fn clearing_drops_emoji_and_expiration() {
        let input = StatusInput {
            text: " ".into(),
            emoji: String::new(),
            expires_at: Some(5),
        };
        let template = template(&input, Some(99), 100).unwrap();
        assert_eq!(template.content, "");
        assert_eq!(template.tags, vec![vec!["d".to_string(), "general".into()]]);
    }

    #[test]
    fn dates_after_the_previous_status() {
        let input = StatusInput::default();
        assert_eq!(template(&input, Some(100), 100).unwrap().created_at, 101);
        assert_eq!(template(&input, Some(50), 100).unwrap().created_at, 100);
        assert_eq!(
            template(&input, Some(400), 100),
            Err(StatusError::ClockSkew)
        );
    }

    #[test]
    fn refuses_long_multiline_and_past_statuses() {
        let long = StatusInput {
            text: "x".repeat(101),
            ..Default::default()
        };
        assert_eq!(template(&long, None, 1), Err(StatusError::TooLong));
        let lines = StatusInput {
            text: "a\nb".into(),
            ..Default::default()
        };
        assert_eq!(template(&lines, None, 1), Err(StatusError::TooLong));
        let past = StatusInput {
            text: "hi".into(),
            expires_at: Some(10),
            ..Default::default()
        };
        assert_eq!(template(&past, None, 10), Err(StatusError::Expired));
    }

    #[test]
    fn reads_the_current_status() {
        let keys = Keys::generate();
        let me = keys.public_key().to_hex();
        let old = event(&keys, "old", 10, &[&["d", "general"]]);
        let new = event(
            &keys,
            "Commuting",
            20,
            &[&["d", "general"], &["emoji", "🚌"], &["expiration", "100"]],
        );
        let scoped = event(&keys, "nope", 30, &[&["d", "general"], &["h", "x"]]);
        let events = [old, new, scoped];
        let status = current(&events, &me, 50).unwrap();
        assert_eq!(status.text, "Commuting");
        assert_eq!(status.emoji, "🚌");
        assert_eq!(status.expires_at, Some(100));
        assert_eq!(latest_created_at(&events, &me), Some(20));
        // Expired.
        assert_eq!(current(&events, &me, 100), None);
        // Cleared.
        let cleared = event(&keys, "", 40, &[&["d", "general"]]);
        assert_eq!(current(&[events[1].clone(), cleared], &me, 50), None);
    }

    #[test]
    fn watched_statuses_load_in_batches_and_keep_the_newest() {
        let viewer = Keys::generate();
        let bob = Keys::generate();
        let carol = Keys::generate();
        let (b, c) = (bob.public_key().to_hex(), carol.public_key().to_hex());
        let mut directory = StatusDirectory::new(viewer.public_key().to_hex());
        assert_eq!(
            directory.watch(&[b.clone(), "nope".into(), b.clone()]),
            vec![b.clone()]
        );
        let read = directory.next_read(0).unwrap();
        assert_eq!(read.authors, vec![b.clone()]);
        // One read at a time.
        assert_eq!(directory.next_read(0), None);
        let newer = event(&bob, "Lunch", 20, &[&["d", "general"], &["emoji", "🍜"]]);
        let older = event(&bob, "Old", 10, &[&["d", "general"]]);
        // Carol is not watched: her status is not kept.
        let stranger = event(&carol, "Hi", 20, &[&["d", "general"]]);
        let events = [newer, older.clone(), stranger.clone()];
        assert_eq!(directory.finished(&read, Ok(&events), 0), Some(true));
        assert_eq!(directory.snapshot()[&b].text, "Lunch");
        assert!(!directory.snapshot().contains_key(&c));
        assert_eq!(directory.next_read(0), None);
        // An older replacement from live traffic does not win.
        assert!(!directory.accept(&[older], 0));
        assert!(!directory.accept(&[stranger], 0));
        // A clear wins, and the status disappears.
        let cleared = event(&bob, "", 30, &[&["d", "general"]]);
        assert!(directory.accept(&[cleared], 0));
        assert!(directory.snapshot().is_empty());
    }

    #[test]
    fn statuses_expire_and_unwatched_people_are_forgotten() {
        let viewer = Keys::generate();
        let bob = Keys::generate();
        let me = viewer.public_key().to_hex();
        let b = bob.public_key().to_hex();
        let mut directory = StatusDirectory::new(me.clone());
        let watched = directory.watch(std::slice::from_ref(&b));
        let second = directory.watch(std::slice::from_ref(&b));
        let expiring = event(
            &bob,
            "Brb",
            10,
            &[&["d", "general"], &["expiration", "100"]],
        );
        // The viewer's own status is always kept.
        let mine = event(&viewer, "Focus", 10, &[&["d", "general"]]);
        assert!(directory.accept(&[expiring, mine], 50_000));
        assert_eq!(directory.snapshot().len(), 2);
        assert_eq!(directory.deadline(), Some(100_000));
        assert!(!directory.tick(99_999));
        assert!(directory.tick(100_000));
        assert_eq!(directory.snapshot().len(), 1);
        assert_eq!(directory.deadline(), None);
        // Watches are counted: one view closing keeps the other's.
        let fresh = event(&bob, "Back", 20, &[&["d", "general"]]);
        assert!(directory.accept(&[fresh], 100_000));
        assert!(!directory.unwatch(&second, 100_000));
        assert!(directory.snapshot().contains_key(&b));
        assert!(directory.unwatch(&watched, 100_000));
        assert!(!directory.snapshot().contains_key(&b));
        assert!(directory.snapshot().contains_key(&me));
        // An access change forgets everything and reads again.
        assert!(directory.clear(100_000));
        assert!(directory.snapshot().is_empty());
    }

    #[test]
    fn failed_reads_retry_three_times_then_wait_for_a_watch() {
        let bob = Keys::generate().public_key().to_hex();
        let mut directory = StatusDirectory::new(Keys::generate().public_key().to_hex());
        directory.watch(std::slice::from_ref(&bob));
        let mut now = 0;
        for delay in RETRY_DELAYS_MS {
            let read = directory.next_read(now).unwrap();
            assert_eq!(directory.finished(&read, Err(()), now), Some(false));
            assert_eq!(directory.deadline(), Some(now + delay));
            assert_eq!(directory.next_read(now + delay - 1), None);
            now += delay;
        }
        let read = directory.next_read(now).unwrap();
        directory.finished(&read, Err(()), now);
        assert_eq!(directory.deadline(), None);
        assert_eq!(directory.next_read(u64::MAX - 1), None);
        // Something new to show tries again.
        directory.watch(std::slice::from_ref(&bob));
        let read = directory.next_read(now).unwrap();
        // A reconnect drops reads already running and reads everyone again.
        directory.reconnect();
        assert_eq!(directory.finished(&read, Ok(&[]), now), None);
        assert_eq!(directory.next_read(now).unwrap().authors, vec![bob]);
    }
}
