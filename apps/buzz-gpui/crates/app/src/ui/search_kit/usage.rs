//! Where the viewer goes in one community, so search can put those places
//! first (`features/search/usage.ts`). Kept in view state under
//! `search-usage`, partitioned like the rest of it by community and viewer,
//! in buzz-app's format; leaving a community clears it. Nothing here goes
//! to the relay.
//!
//! Destination keys: `channel:<id>` for a conversation, the page key (such
//! as `buzz.inbox/inbox`) for a page, and `settings`.

use crate::community::{AppState, Community};
use crate::ui::navigation::Place;
use buzz_relay_client::Store;
use buzz_relay_client::store::Partition;
use gpui_kit::{App, Entity};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const KEY: &str = "search-usage";
const LIMIT: usize = 200;
/// A visit is worth half as much after a week.
pub const HALF_LIFE_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;
/// A search choice also opens its destination, which counts one visit.
const CHOICE_WEIGHT: f64 = 2.0;
/// The most a usage score can lift a match: past a slightly better match
/// (one rank), never past a much better one (two ranks).
pub const MAX_BOOST: f64 = 1.5;

/// The stored record: decayed visit scores and the last choice per typed
/// text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stored {
    /// `(destination key, decayed score, score time in ms)`, best first.
    pub visits: Vec<(String, f64, f64)>,
    /// `(typed text, destination key, time in ms)`, newest first.
    pub picks: Vec<(String, String, f64)>,
}

fn decayed(score: f64, at: f64, now: f64) -> f64 {
    score * 2f64.powf(-(now - at).max(0.0) / HALF_LIFE_MS)
}

impl Stored {
    /// Reads the stored JSON, keeping only well-formed entries.
    pub fn parse(json: &str) -> Self {
        let value: Value = serde_json::from_str(json).unwrap_or(Value::Null);
        let entries = |name: &str| -> Vec<Value> {
            value
                .get(name)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let finite = |value: &Value| value.as_f64().filter(|n| n.is_finite());
        let visits = entries("visits")
            .iter()
            .filter_map(|entry| match entry.as_array()?.as_slice() {
                [key, score, at] => Some((key.as_str()?.to_owned(), finite(score)?, finite(at)?)),
                _ => None,
            })
            .collect();
        let picks = entries("picks")
            .iter()
            .filter_map(|entry| match entry.as_array()?.as_slice() {
                [text, key, at] => Some((
                    text.as_str()?.to_owned(),
                    key.as_str()?.to_owned(),
                    finite(at)?,
                )),
                _ => None,
            })
            .collect();
        Self { visits, picks }
    }

    pub fn to_json(&self) -> String {
        json!({ "version": 1, "visits": self.visits, "picks": self.picks }).to_string()
    }

    fn add_visit(&mut self, key: &str, weight: f64, now: f64) {
        let previous = self.visits.iter().find(|(visited, ..)| visited == key);
        let score = previous.map_or(0.0, |(_, score, at)| decayed(*score, *at, now)) + weight;
        self.visits.retain(|(visited, ..)| visited != key);
        self.visits.insert(0, (key.to_owned(), score, now));
        self.visits.sort_by(|a, b| {
            decayed(b.1, b.2, now)
                .partial_cmp(&decayed(a.1, a.2, now))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.visits.truncate(LIMIT);
    }

    /// Counts one open of `key`, from anywhere in the app.
    pub fn visit(&mut self, key: &str, now: f64) {
        self.add_visit(key, 1.0, now);
    }

    /// Remembers that `typed` led to `key`, replacing the earlier choice
    /// for that text. A search choice counts more than an ordinary open;
    /// the open that follows adds its own visit.
    pub fn choose(&mut self, typed: &str, key: &str, now: f64) {
        self.add_visit(key, CHOICE_WEIGHT, now);
        if !typed.is_empty() {
            self.picks.retain(|(text, ..)| text != typed);
            self.picks
                .insert(0, (typed.to_owned(), key.to_owned(), now));
            self.picks.truncate(LIMIT);
        }
    }

    /// The scores as of `now`, for ranking.
    pub fn usage(&self, now: f64) -> SearchUsage {
        SearchUsage {
            scores: self
                .visits
                .iter()
                .map(|(key, score, at)| (key.clone(), decayed(*score, *at, now)))
                .collect(),
            picks: self.picks.clone(),
        }
    }
}

/// Usage as read once when a picker opens, so usage recorded while it is
/// open cannot reorder it.
#[derive(Clone, Debug, Default)]
pub struct SearchUsage {
    scores: HashMap<String, f64>,
    picks: Vec<(String, String, f64)>,
}

impl SearchUsage {
    /// No usage: every boost is 0 and nothing was picked.
    #[allow(dead_code)] // For pickers other than Command-K.
    pub fn none() -> Self {
        Self::default()
    }

    /// How far usage lifts a match's rank, from 0 up to [`MAX_BOOST`],
    /// half the maximum at four visits' worth of score. Several keys for
    /// one row, such as a person and their DM, share one score.
    pub fn boost(&self, keys: &[&str]) -> f64 {
        let score: f64 = keys
            .iter()
            .map(|key| self.scores.get(*key).copied().unwrap_or(0.0))
            .sum();
        MAX_BOOST * score / (score + 4.0)
    }

    /// The destination last chosen for this typed text, a longer text that
    /// starts with it, or a shorter text it starts with, among
    /// `candidates`.
    pub fn pick(&self, typed: &str, candidates: &HashSet<String>) -> Option<String> {
        if typed.is_empty() {
            return None;
        }
        let related: Vec<&(String, String, f64)> = self
            .picks
            .iter()
            .filter(|(text, key, _)| {
                candidates.contains(key) && (text.starts_with(typed) || typed.starts_with(text))
            })
            .collect();
        related
            .iter()
            .find(|(text, ..)| text == typed)
            .or_else(|| {
                related
                    .iter()
                    .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
            })
            .map(|(_, key, _)| key.clone())
    }
}

/// Typed text in a picker other than Command-K. Each picker keeps its own
/// earlier choices, so a choice in one cannot replace another's for the
/// same text. The prefix cannot be typed, so Command-K text never matches
/// it.
#[allow(dead_code)] // For pickers other than Command-K.
pub fn picker_text(picker: &str, typed: &str) -> String {
    if typed.is_empty() {
        String::new()
    } else {
        format!("\u{0}{picker}\u{0}{typed}")
    }
}

/// The one search order for picker rows; lower sorts first. An exact name
/// (match rank 0) leads, then the viewer's earlier choice for this typed
/// text, then the match rank lifted by usage: past a slightly better
/// match, never past a much better one.
pub fn search_order(usage: &SearchUsage, picked: Option<&str>, key: &str, rank: f64) -> f64 {
    search_order_in(usage, picked, key, rank, 0.0, &[key])
}

/// [`search_order`] with a `band`, which groups the lifted ranks first, ten
/// apart (as New message recipients do by relationship); exact and chosen
/// rows stay ahead of every band. `usage_keys` are the keys whose usage
/// lifts the row.
pub fn search_order_in(
    usage: &SearchUsage,
    picked: Option<&str>,
    key: &str,
    rank: f64,
    band: f64,
    usage_keys: &[&str],
) -> f64 {
    if rank == 0.0 {
        return -2.0;
    }
    if picked == Some(key) {
        return -1.0;
    }
    band * 10.0 + rank - usage.boost(usage_keys)
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// The stored record for one community and viewer.
pub fn read(store: &Store, partition: &Partition) -> Stored {
    store
        .view_state(partition, KEY)
        .ok()
        .flatten()
        .map(|json| Stored::parse(&json))
        .unwrap_or_default()
}

fn update(store: &Store, partition: &Partition, change: impl FnOnce(&mut Stored)) {
    let mut stored = read(store, partition);
    change(&mut stored);
    if let Err(error) = store.set_view_state(partition, KEY, Some(&stored.to_json())) {
        tracing::warn!(%error, "could not save search usage");
    }
}

/// The usage of the viewer in `community`, as of now.
pub fn usage_of(community: &Community, cx: &App) -> SearchUsage {
    read(&cx.global::<AppState>().store, &community.partition()).usage(now_ms())
}

/// Remembers that typed text in a picker led to `key`. Pickers other than
/// Command-K pass their text through [`picker_text`].
pub fn record_choice(community: &Community, typed: &str, key: &str, cx: &App) {
    let store = &cx.global::<AppState>().store;
    update(store, &community.partition(), |stored| {
        stored.choose(typed, key, now_ms())
    });
}

/// The destination key of a place in the window.
pub fn visit_key(place: &Place) -> String {
    match place {
        Place::Conversation { channel, .. } => format!("channel:{channel}"),
        Place::Page(page) | Place::Route { page, .. } => page.key().to_owned(),
        Place::Settings(_) => crate::ui::pages::Page::SETTINGS.key().to_owned(),
        Place::Unresolved(request) => request
            .get("page")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        // Messages without a conversation open.
        Place::Channels(_) => crate::ui::pages::Page::MESSAGES.key().to_owned(),
    }
}

/// Counts a completed open of `place` in `community` as a visit: sidebar,
/// links, notifications, history and search alike. The workspace calls it
/// when it records navigation. A `restore` (the place already showing at
/// startup) is not a visit, and nothing is counted without a community.
pub fn record_visit(community: Option<&Entity<Community>>, place: &Place, restore: bool, cx: &App) {
    let Some(community) = community.filter(|_| !restore) else {
        return;
    };
    let store = &cx.global::<AppState>().store;
    let key = visit_key(place);
    update(store, &community.read(cx).partition(), |stored| {
        stored.visit(&key, now_ms())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::pages::Page;

    const NOW: f64 = 1_000_000_000_000.0;

    fn set(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|key| key.to_string()).collect()
    }

    #[test]
    fn visits_decay_with_a_week_half_life() {
        let mut stored = Stored::default();
        stored.visit("channel:a", NOW - HALF_LIFE_MS);
        let usage = stored.usage(NOW);
        let one_old_visit = usage.boost(&["channel:a"]);
        // Half a visit: 1.5 × 0.5 / 4.5.
        assert!((one_old_visit - MAX_BOOST * 0.5 / 4.5).abs() < 1e-9);
        assert_eq!(usage.boost(&["channel:b"]), 0.0);
    }

    #[test]
    fn boost_is_half_the_maximum_at_four_visits_and_never_reaches_it() {
        let mut stored = Stored::default();
        for _ in 0..4 {
            stored.visit("x", NOW);
        }
        let usage = stored.usage(NOW);
        assert!((usage.boost(&["x"]) - MAX_BOOST / 2.0).abs() < 1e-9);
        for _ in 0..1000 {
            stored.visit("x", NOW);
        }
        assert!(stored.usage(NOW).boost(&["x"]) < MAX_BOOST);
        // Several keys share one score.
        let mut pair = Stored::default();
        pair.visit("person", NOW);
        pair.visit("dm", NOW);
        assert!((pair.usage(NOW).boost(&["person", "dm"]) - MAX_BOOST * 2.0 / 6.0).abs() < 1e-9);
    }

    #[test]
    fn a_choice_counts_two_visits_and_remembers_the_text() {
        let mut stored = Stored::default();
        stored.choose("gen", "channel:general", NOW);
        assert_eq!(stored.visits, [("channel:general".into(), 2.0, NOW)]);
        stored.choose("gen", "channel:genesis", NOW + 1.0);
        assert_eq!(
            stored.picks,
            [("gen".into(), "channel:genesis".into(), NOW + 1.0)]
        );
        // Empty text records the visit only.
        stored.choose("", "settings", NOW);
        assert_eq!(stored.picks.len(), 1);
    }

    #[test]
    fn picks_follow_related_text_among_candidates() {
        let mut stored = Stored::default();
        stored.choose("ge", "channel:a", NOW);
        stored.choose("gene", "channel:b", NOW + 1.0);
        let usage = stored.usage(NOW);
        let both = set(&["channel:a", "channel:b"]);
        // The same text wins.
        assert_eq!(usage.pick("ge", &both).as_deref(), Some("channel:a"));
        // Otherwise the newest related text: longer or shorter.
        assert_eq!(usage.pick("gen", &both).as_deref(), Some("channel:b"));
        assert_eq!(
            usage.pick("generic", &set(&["channel:a"])).as_deref(),
            Some("channel:a")
        );
        // A stale choice cannot add a row.
        assert_eq!(usage.pick("gen", &set(&["channel:c"])), None);
        assert_eq!(usage.pick("", &both), None);
        assert_eq!(usage.pick("x", &both), None);
    }

    #[test]
    fn orders_exact_then_picked_then_lifted_rank() {
        let mut stored = Stored::default();
        for _ in 0..40 {
            stored.visit("often", NOW);
        }
        let usage = stored.usage(NOW);
        assert_eq!(search_order(&usage, None, "any", 0.0), -2.0);
        assert_eq!(search_order(&usage, Some("mine"), "mine", 3.0), -1.0);
        // Usage lifts past one rank, never two.
        let often = search_order(&usage, None, "often", 3.0);
        assert!(often < search_order(&usage, None, "never", 2.0));
        assert!(often > search_order(&usage, None, "never", 1.0));
        // Bands keep their order.
        assert!(
            search_order_in(&usage, None, "often", 1.0, 1.0, &["often"])
                > search_order_in(&usage, None, "never", 5.0, 0.0, &["never"])
        );
    }

    #[test]
    fn picker_text_is_separate_from_command_k() {
        assert_eq!(picker_text("tabs", ""), "");
        let text = picker_text("tabs", "gen");
        assert_eq!(text, "\u{0}tabs\u{0}gen");
        assert!(!text.starts_with("gen"));
    }

    #[test]
    fn keeps_the_two_hundred_best_visits() {
        let mut stored = Stored::default();
        for n in 0..250 {
            stored.visit(&format!("k{n}"), NOW + f64::from(n));
        }
        stored.visit("k249", NOW + 300.0);
        assert_eq!(stored.visits.len(), LIMIT);
        assert_eq!(stored.visits[0].0, "k249");
    }

    #[test]
    fn round_trips_buzz_app_json_and_drops_bad_entries() {
        let json = r#"{"version":1,"visits":[["channel:a",2,5],["bad",1],[3,1,1]],"picks":[["ge","channel:a",5],["x"]]}"#;
        let stored = Stored::parse(json);
        assert_eq!(stored.visits, [("channel:a".into(), 2.0, 5.0)]);
        assert_eq!(stored.picks, [("ge".into(), "channel:a".into(), 5.0)]);
        assert_eq!(Stored::parse(&stored.to_json()), stored);
        assert_eq!(Stored::parse("not json"), Stored::default());
    }

    #[test]
    fn persists_per_community_and_viewer() {
        let store = Store::in_memory().expect("store");
        let origin = buzz_relay_client::RelayOrigin::parse("wss://a.test").expect("origin");
        let mine = Partition::new(&origin, &"a".repeat(64));
        let theirs = Partition::new(&origin, &"b".repeat(64));
        update(&store, &mine, |stored| {
            stored.choose("ge", "channel:a", NOW)
        });
        assert_eq!(read(&store, &mine).picks.len(), 1);
        assert_eq!(read(&store, &theirs), Stored::default());
    }

    #[test]
    fn keys_places_like_buzz_app() {
        let channel = Place::Conversation {
            channel: "c1".into(),
            message: None,
            thread: None,
            members: false,
        };
        assert_eq!(visit_key(&channel), "channel:c1");
        assert_eq!(visit_key(&Place::Page(Page::SETTINGS)), "settings");
        assert_eq!(visit_key(&Place::Page(Page::AGENTS)), "buzz.agents/agents");
    }
}
