//! The community's custom emoji catalog (`emoji-directory.ts`,
//! `docs/port/relay.md` §5k.1-5k.2).
//!
//! Every member publishes one kind 30030 set with `d = buzz:custom-emoji`;
//! the palette is the union of the newest set from each member. For each
//! shortcode the newest set wins, and on a tie the smaller URL. Sets are
//! never evicted: forgetting a replacement could bring back a code its
//! author removed. A catalog over budget shows an error and no entries
//! until the next full read.

use crate::event::{Verified, is_newer, kinds, newest};
use crate::messages::{CustomEmoji, EmojiCatalog, EventTemplate, normalize_shortcode};
use crate::socket::{Filter, filter};
use nostr::prelude::Event;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The `d` tag of every member's set.
pub const EMOJI_SET: &str = "buzz:custom-emoji";
/// At most this many member sets, and the read's limit.
const LIMIT: usize = 500;
/// Total bytes of the retained sets.
const MAX_BYTES: usize = 2 * 1024 * 1024;

const OVER_BUDGET: &str =
    "Community emoji exceeds the catalog budget. Retry after reducing the catalog.";
const INCOMPLETE: &str = "Community emoji catalog may be incomplete (500-set read limit). Retry after reducing the catalog.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogStatus {
    /// Not read yet.
    Idle,
    Loading,
    Ready,
    Error(String),
}

/// What the UI and the message builders see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmojiSnapshot {
    pub status: CatalogStatus,
    /// The merged palette, sorted by shortcode.
    pub entries: Arc<[CustomEmoji]>,
    /// The viewer's own latest set.
    pub mine: Arc<[CustomEmoji]>,
}

impl Default for EmojiSnapshot {
    fn default() -> Self {
        Self {
            status: CatalogStatus::Idle,
            entries: Arc::new([]),
            mine: Arc::new([]),
        }
    }
}

impl EmojiSnapshot {
    /// The catalog as a send needs it: `:code:` text can only be resolved
    /// once the whole palette is known.
    pub fn catalog(&self) -> EmojiCatalog<'_> {
        match &self.status {
            CatalogStatus::Ready => EmojiCatalog::Ready(&self.entries),
            CatalogStatus::Error(error) => EmojiCatalog::Unavailable(error),
            CatalogStatus::Idle | CatalogStatus::Loading => EmojiCatalog::Loading,
        }
    }

    pub fn get(&self, shortcode: &str) -> Option<&CustomEmoji> {
        self.entries
            .binary_search_by(|entry| entry.shortcode.as_str().cmp(shortcode))
            .ok()
            .map(|index| &self.entries[index])
    }
}

/// The full catalog read (`emoji-directory.ts:111-150`).
pub fn catalog_filter() -> Filter {
    filter(json!({"kinds": [kinds::CUSTOM_EMOJI], "#d": [EMOJI_SET], "limit": LIMIT}))
}

/// The viewer's own set, read fresh before changing it (§5k.3).
pub fn own_set_filter(viewer: &str) -> Filter {
    filter(
        json!({"kinds": [kinds::CUSTOM_EMOJI], "#d": [EMOJI_SET], "authors": [viewer], "limit": 1}),
    )
}

/// The newest of `viewer`'s sets among `events`.
pub fn own_set<'a>(events: &'a [Verified], viewer: &str) -> Option<&'a Verified> {
    newest(
        events
            .iter()
            .filter(|event| is_set(event) && event.pubkey_hex() == viewer),
    )
}

pub const INVALID_NAME: &str = "Invalid emoji name. Use letters, numbers, hyphen, or underscore.";
const INVALID_URL: &str = "Choose an image file for custom emoji.";
const MAX_URL: usize = 2048;

/// The set that adds `code` (or replaces its image): every current entry
/// except `code`, then `code` last, dated after the current set
/// (`emoji-directory.ts:155-227`).
pub fn add_template(
    current: Option<&Event>,
    code: &str,
    url: &str,
    now: u64,
) -> Result<EventTemplate, &'static str> {
    let code = normalize_shortcode(code).ok_or(INVALID_NAME)?;
    let url_ok = !url.is_empty()
        && url.trim() == url
        && url.len() <= MAX_URL
        && (url.starts_with("https://") || url.starts_with("http://"));
    if !url_ok {
        return Err(INVALID_URL);
    }
    let mut tags = vec![vec!["d".to_owned(), EMOJI_SET.to_owned()]];
    for entry in current.map(set_entries).unwrap_or_default() {
        if entry.shortcode != code {
            tags.push(vec!["emoji".to_owned(), entry.shortcode, entry.url]);
        }
    }
    tags.push(vec!["emoji".to_owned(), code, url.to_owned()]);
    let created_at = current.map_or(now, |event| now.max(event.created_at.as_secs() + 1));
    Ok(EventTemplate {
        kind: kinds::CUSTOM_EMOJI,
        created_at,
        tags,
        content: String::new(),
    })
}

/// `validEmojiSetTemplate`: what the viewer's key signs as an emoji set —
/// one coordinate, canonical unique codes, bounded links, and a date at
/// most five minutes ahead.
pub fn valid_set_template(template: &EventTemplate, now: u64) -> bool {
    if template.kind != kinds::CUSTOM_EMOJI
        || !template.content.is_empty()
        || template.created_at > now + 300
        || template.tags.first().map(Vec::as_slice) != Some(&["d".to_owned(), EMOJI_SET.to_owned()])
    {
        return false;
    }
    let mut codes = std::collections::HashSet::new();
    template.tags[1..].iter().all(|tag| {
        let [name, code, url] = tag.as_slice() else {
            return false;
        };
        name == "emoji"
            && crate::messages::normalize_shortcode(code).as_deref() == Some(code.as_str())
            && codes.insert(code.clone())
            && !url.is_empty()
            && url.trim() == url
            && url.len() <= MAX_URL
    })
}

/// `suggestShortcode`: the file name without its directory and extension,
/// lowercase, invalid runs as one `_`, trimmed of `_` and `-`.
pub fn suggest_shortcode(file_name: &str) -> Option<String> {
    let name = file_name.trim();
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = name.rfind('.').map_or(name, |dot| &name[..dot]);
    let mut code = String::new();
    for c in stem.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
            if !(c == '_' && code.ends_with('_')) {
                code.push(c);
            }
        } else if !code.ends_with('_') {
            code.push('_');
        }
    }
    normalize_shortcode(code.trim_matches(|c| c == '_' || c == '-'))
}

/// `emojiTags`: the set's `["emoji", code, url]` entries, codes normalized,
/// first per code wins.
pub fn set_entries(event: &Event) -> Vec<CustomEmoji> {
    let mut entries: Vec<CustomEmoji> = Vec::new();
    let mut seen = HashSet::new();
    for tag in event.tags.iter().map(|tag| tag.as_slice()) {
        let [name, code, url, ..] = tag else {
            continue;
        };
        if name != "emoji" || url.trim().is_empty() {
            continue;
        }
        let Some(shortcode) = normalize_shortcode(code) else {
            continue;
        };
        if seen.insert(shortcode.clone()) {
            entries.push(CustomEmoji {
                shortcode,
                url: url.trim().to_owned(),
            });
        }
    }
    entries
}

fn is_set(event: &Event) -> bool {
    event.kind.as_u16() == kinds::CUSTOM_EMOJI
        && event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .find(|tag| tag.first().map(String::as_str) == Some("d"))
            .and_then(|tag| tag.get(1))
            .is_some_and(|d| d == EMOJI_SET)
}

/// Each member's newest set and the merged palette.
pub struct EmojiDirectory {
    viewer: String,
    sets: HashMap<String, (Verified, usize)>,
    bytes: usize,
    /// Over budget: entries stay empty until a full read starts again.
    limited: bool,
    snapshot: Arc<EmojiSnapshot>,
}

impl EmojiDirectory {
    pub fn new(viewer: String) -> Self {
        Self {
            viewer,
            sets: HashMap::new(),
            bytes: 0,
            limited: false,
            snapshot: Arc::default(),
        }
    }

    pub fn snapshot(&self) -> Arc<EmojiSnapshot> {
        self.snapshot.clone()
    }

    /// The viewer's newest set the directory holds.
    pub fn own(&self) -> Option<Verified> {
        self.sets.get(&self.viewer).map(|(event, _)| event.clone())
    }

    /// A full read is starting. An over-budget catalog starts over, and is
    /// never shown as ready while partly retained.
    pub fn loading(&mut self) -> bool {
        if self.limited {
            self.sets.clear();
            self.bytes = 0;
            self.limited = false;
        }
        self.set_status(CatalogStatus::Loading)
    }

    /// The full read's outcome. Events go through [`Self::accept`] too.
    pub fn finished(&mut self, result: Result<&[Verified], String>) -> bool {
        match result {
            Ok(events) => {
                let mut changed = self.accept(events);
                if self.limited {
                    return changed;
                }
                changed |= if events.len() >= LIMIT {
                    self.set_status(CatalogStatus::Error(INCOMPLETE.into()))
                } else {
                    self.set_status(CatalogStatus::Ready)
                };
                changed
            }
            Err(error) => self.set_status(CatalogStatus::Error(error)),
        }
    }

    /// Takes newer member sets from any source. Returns whether the
    /// snapshot changed.
    pub fn accept(&mut self, events: &[Verified]) -> bool {
        if self.limited {
            return false;
        }
        let mut changed = false;
        for event in events.iter().filter(|event| is_set(event)) {
            let author = event.pubkey_hex();
            let old = self.sets.get(&author);
            if old.is_some_and(|(current, _)| !is_newer(event, current)) {
                continue;
            }
            let size = event.byte_size();
            let old_size = old.map_or(0, |(_, size)| *size);
            if (old.is_none() && self.sets.len() >= LIMIT)
                || self.bytes - old_size + size > MAX_BYTES
            {
                self.limited = true;
                self.snapshot = Arc::new(EmojiSnapshot {
                    status: CatalogStatus::Error(OVER_BUDGET.into()),
                    entries: Arc::new([]),
                    mine: Arc::new([]),
                });
                return true;
            }
            self.bytes = self.bytes - old_size + size;
            self.sets.insert(author, (event.clone(), size));
            changed = true;
        }
        if changed {
            self.merge();
        }
        changed
    }

    fn merge(&mut self) {
        let mut winners: HashMap<String, (CustomEmoji, u64)> = HashMap::new();
        for (event, _) in self.sets.values() {
            let time = event.created_at_secs();
            for emoji in set_entries(event) {
                let replace = winners
                    .get(&emoji.shortcode)
                    .is_none_or(|(old, at)| time > *at || (time == *at && emoji.url < old.url));
                if replace {
                    winners.insert(emoji.shortcode.clone(), (emoji, time));
                }
            }
        }
        let mut entries: Vec<CustomEmoji> = winners.into_values().map(|(emoji, _)| emoji).collect();
        entries.sort_by(|a, b| a.shortcode.cmp(&b.shortcode));
        let mine = self
            .sets
            .get(&self.viewer)
            .map(|(event, _)| set_entries(event))
            .unwrap_or_default();
        self.snapshot = Arc::new(EmojiSnapshot {
            status: self.snapshot.status.clone(),
            entries: entries.into(),
            mine: mine.into(),
        });
    }

    fn set_status(&mut self, status: CatalogStatus) -> bool {
        if self.snapshot.status == status {
            return false;
        }
        self.snapshot = Arc::new(EmojiSnapshot {
            status,
            ..(*self.snapshot).clone()
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Verifier;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};

    fn set(keys: &Keys, at: u64, entries: &[(&str, &str)]) -> Verified {
        let mut tags = vec![Tag::parse(["d", EMOJI_SET]).unwrap()];
        tags.extend(
            entries
                .iter()
                .map(|(code, url)| Tag::parse(["emoji", code, url]).unwrap()),
        );
        let event = EventBuilder::new(Kind::from_u16(kinds::CUSTOM_EMOJI), "")
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(keys)
            .unwrap();
        Verifier::default().verify(event).unwrap()
    }

    #[test]
    fn adds_or_replaces_one_entry_in_the_viewer_set() {
        let alice = Keys::generate();
        let current = set(
            &alice,
            500,
            &[
                ("party", "https://x.test/p.gif"),
                ("ship", "https://x.test/s.png"),
            ],
        );
        let template =
            add_template(Some(&current), ":Party:", "https://x.test/p2.gif", 100).unwrap();
        assert_eq!(template.kind, kinds::CUSTOM_EMOJI);
        assert_eq!(template.content, "");
        // Dated after the set it replaces, even with a slow clock.
        assert_eq!(template.created_at, 501);
        assert_eq!(
            template.tags,
            [
                vec!["d", EMOJI_SET],
                vec!["emoji", "ship", "https://x.test/s.png"],
                vec!["emoji", "party", "https://x.test/p2.gif"],
            ]
        );
        let first = add_template(None, "wave", "https://x.test/w.png", 900).unwrap();
        assert_eq!(first.created_at, 900);
        assert_eq!(first.tags.len(), 2);
        assert_eq!(
            add_template(None, "no spaces", "https://x.test/w.png", 900),
            Err(INVALID_NAME)
        );
        assert!(add_template(None, "ok", " https://x.test/w.png", 900).is_err());
        assert!(add_template(None, "ok", "data:image/png;base64,AA", 900).is_err());
        // What the key signs: canonical unique codes, dated near now.
        assert!(valid_set_template(&template, 201));
        assert!(
            !valid_set_template(&template, 200),
            "over five minutes ahead"
        );
        let mut twice = first.clone();
        twice.tags.push(twice.tags[1].clone());
        assert!(!valid_set_template(&twice, 900));
        let mut loud = first;
        loud.tags[1][1] = "Wave".into();
        assert!(!valid_set_template(&loud, 900));
    }

    #[test]
    fn suggests_shortcodes_from_file_names_like_the_reference() {
        assert_eq!(
            suggest_shortcode("Party Parrot!.GIF").as_deref(),
            Some("party_parrot")
        );
        assert_eq!(
            suggest_shortcode("/tmp/shots/ship-it.v2.png").as_deref(),
            Some("ship-it_v2")
        );
        assert_eq!(suggest_shortcode("__done__.webp").as_deref(), Some("done"));
        assert_eq!(suggest_shortcode("日本.png"), None);
    }

    fn codes(snapshot: &EmojiSnapshot) -> Vec<(String, String)> {
        snapshot
            .entries
            .iter()
            .map(|e| (e.shortcode.clone(), e.url.clone()))
            .collect()
    }

    #[test]
    fn merges_member_sets_newest_first_then_smaller_url() {
        let (alice, bob) = (Keys::generate(), Keys::generate());
        let mut directory = EmojiDirectory::new(alice.public_key().to_hex());
        assert!(matches!(
            directory.snapshot().catalog(),
            EmojiCatalog::Loading
        ));
        directory.loading();
        let events = [
            set(
                &alice,
                10,
                &[("party", "https://a/p.png"), ("Ship", "https://a/s.png")],
            ),
            set(&bob, 20, &[("party", "https://b/p.png")]),
        ];
        assert!(directory.finished(Ok(&events)));
        let snapshot = directory.snapshot();
        assert_eq!(snapshot.status, CatalogStatus::Ready);
        assert_eq!(
            codes(&snapshot),
            [
                ("party".into(), "https://b/p.png".into()),
                ("ship".into(), "https://a/s.png".into())
            ]
        );
        assert_eq!(snapshot.mine.len(), 2);
        assert_eq!(snapshot.get("ship").unwrap().url, "https://a/s.png");

        // An older set from the same author changes nothing; a tie on time
        // goes to the smaller URL.
        assert!(!directory.accept(&[set(&bob, 5, &[])]));
        let carol = Keys::generate();
        assert!(directory.accept(&[set(&carol, 20, &[("party", "https://a/0.png")])]));
        assert_eq!(
            directory.snapshot().get("party").unwrap().url,
            "https://a/0.png"
        );

        // Bob's newer, empty set removes nothing that others still have.
        assert!(directory.accept(&[set(&bob, 30, &[])]));
        assert_eq!(directory.snapshot().entries.len(), 2);
    }

    #[test]
    fn over_budget_catalogs_show_an_error_until_reread() {
        let mut directory = EmojiDirectory::new(String::new());
        directory.loading();
        let big = "x".repeat(4000);
        let entries: Vec<(String, String)> = (0..600)
            .map(|i| (format!("e{i}"), format!("https://a/{big}{i}")))
            .collect();
        let refs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let huge = set(&Keys::generate(), 1, &refs);
        directory.finished(Ok(&[huge]));
        let snapshot = directory.snapshot();
        assert!(matches!(snapshot.status, CatalogStatus::Error(_)));
        assert!(snapshot.entries.is_empty());
        assert!(!directory.accept(&[set(&Keys::generate(), 2, &[("a", "https://a")])]));
        directory.loading();
        assert_eq!(directory.snapshot().status, CatalogStatus::Loading);
    }

    #[test]
    fn reads_set_entries_like_the_reference() {
        let keys = Keys::generate();
        let event = set(
            &keys,
            1,
            &[
                ("Party", " https://a "),
                ("party", "https://b"),
                ("bad code", "https://c"),
                ("x", " "),
            ],
        );
        assert_eq!(
            set_entries(&event),
            [CustomEmoji {
                shortcode: "party".into(),
                url: "https://a".into()
            }]
        );
    }
}
