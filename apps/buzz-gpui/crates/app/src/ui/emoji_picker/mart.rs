//! The parts of emoji-mart (5.6, `@emoji-mart/data` 1.2.1 set 15) that
//! buzz-app's picker behaviour rests on: Mart's search index and result
//! order with buzz-app's fuzzy fill (`emoji-mart.ts:566-594`), the
//! "Frequently used" history (`FrequentlyUsed`), keyboard movement
//! through the grid (`navigate`), and the responsive layout
//! (`EmojiPicker.tsx:103-139`). No GPUI here, so it is all testable.
//!
//! `emoji-mart.tsv` is written by scripts/emoji-mart-data.py.

use super::Candidate;
use buzz_relay_client::CustomEmoji;
use emojis::{Emoji, SkinTone};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

/// Mart's default `maxResults`.
pub const MAX_RESULTS: usize = 90;
/// `maxFrequentRows`.
pub const FREQUENT_ROWS: usize = 2;

/// One emoji in Mart's dictionary.
pub struct MartEmoji {
    pub id: &'static str,
    pub emoji: &'static Emoji,
    /// `,id,name words,keywords,emoticons,natives`: Mart scores a match by
    /// where it starts in this string.
    search: String,
    aliases: Vec<&'static str>,
}

const TONES: [SkinTone; 5] = [
    SkinTone::Light,
    SkinTone::MediumLight,
    SkinTone::Medium,
    SkinTone::MediumDark,
    SkinTone::Dark,
];

static DATA: LazyLock<Vec<MartEmoji>> = LazyLock::new(|| {
    include_str!("emoji-mart.tsv")
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (id, native, skins, terms, aliases) = (
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next().unwrap_or_default(),
            );
            let emoji = emojis::get(native)?;
            let mut search = format!(",{id}");
            if !terms.is_empty() {
                search.push(',');
                search.push_str(terms);
            }
            search.push(',');
            search.push_str(native);
            if skins == "6" {
                for tone in TONES {
                    if let Some(toned) = emoji.with_skin_tone(tone) {
                        search.push(',');
                        search.push_str(toned.as_str());
                    }
                }
            }
            Some(MartEmoji {
                id,
                emoji,
                search,
                aliases: aliases.split(',').filter(|a| !a.is_empty()).collect(),
            })
        })
        .collect()
});

static BY_ID: LazyLock<HashMap<&'static str, &'static MartEmoji>> =
    LazyLock::new(|| DATA.iter().map(|entry| (entry.id, entry)).collect());

static BY_EMOJI: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    DATA.iter()
        .map(|entry| (entry.emoji.as_str(), entry.id))
        .collect()
});

/// Mart's id for an emoji, e.g. `+1` for 👍.
pub fn id_of(emoji: &Emoji) -> Option<&'static str> {
    BY_EMOJI.get(emoji.as_str()).copied()
}

pub fn by_id(id: &str) -> Option<&'static Emoji> {
    BY_ID.get(id).map(|entry| entry.emoji)
}

/// The id Mart gives a community's custom emoji (`emoji-mart.ts:73`),
/// so history stays per community.
pub fn custom_id(scope: &str, shortcode: &str) -> String {
    format!("buzz-custom/{scope}/{shortcode}")
}

/// `String.prototype.localeCompare` for these ASCII ids: ICU's root
/// order puts punctuation, then symbols, then digits, then letters.
pub fn locale_cmp(a: &str, b: &str) -> Ordering {
    fn weight(c: char) -> (u8, char) {
        let class = match c {
            '_' => 0,
            '-' => 1,
            '+' | '<' | '=' | '>' | '|' | '~' | '$' | '^' | '`' => 3,
            c if c.is_ascii_digit() => 4,
            c if c.is_alphabetic() => 5,
            c if c.is_ascii() => 2,
            _ => 6,
        };
        (class, c.to_ascii_lowercase())
    }
    a.chars().map(weight).cmp(b.chars().map(weight))
}

/// JavaScript's `split` on a `/[…]+/` pattern: runs of separators
/// split once, and an edge separator leaves an empty piece.
fn split_runs(value: &str, separator: impl Fn(char) -> bool) -> Vec<&str> {
    let pieces: Vec<&str> = value.split(separator).collect();
    let last = pieces.len() - 1;
    pieces
        .into_iter()
        .enumerate()
        .filter(|(index, piece)| !piece.is_empty() || *index == 0 || *index == last)
        .map(|(_, piece)| piece)
        .collect()
}

/// `replace(/(\w)-/, "$1 ")`: the first hyphen after a word character.
fn space_first_hyphen(value: &str) -> String {
    let mut out = value.to_string();
    let mut previous: Option<char> = None;
    for (index, c) in value.char_indices() {
        if c == '-' && previous.is_some_and(|p| p.is_ascii_alphanumeric() || p == '_') {
            out.replace_range(index..index + 1, " ");
            break;
        }
        previous = Some(c);
    }
    out
}

fn query_separator(c: char) -> bool {
    c.is_whitespace() || c == '|' || c == ','
}

/// One searchable entry: Mart's own emoji or a custom one.
struct Item<'a> {
    id: String,
    search: std::borrow::Cow<'a, str>,
    candidate: Candidate,
    /// Mart ids and aliases (or the custom shortcode) for the fuzzy fill.
    shortcodes: Vec<&'a str>,
}

fn items<'a>(scope: &str, custom: &'a [CustomEmoji]) -> Vec<Item<'a>> {
    let mut items: Vec<Item> = DATA
        .iter()
        .map(|entry| Item {
            id: entry.id.to_string(),
            search: entry.search.as_str().into(),
            candidate: Candidate::Unicode(entry.emoji),
            shortcodes: std::iter::once(entry.id)
                .chain(entry.aliases.iter().copied())
                .collect(),
        })
        .collect();
    items.extend(custom.iter().map(|emoji| {
        let code = emoji.shortcode.as_str();
        let literal = format!(":{code}:");
        // Mart's first-hyphen query normalization and individual words.
        let mut terms = Vec::new();
        for name in [code, literal.as_str()] {
            terms.push(name.to_string());
            terms.extend(
                split_runs(&space_first_hyphen(name), query_separator)
                    .into_iter()
                    .map(str::to_string),
            );
            terms.extend(
                split_runs(name, |c| c == '-' || c == '_')
                    .into_iter()
                    .map(str::to_string),
            );
        }
        Item {
            id: custom_id(scope, code),
            search: format!(",{}", terms.join(",")).into(),
            candidate: Candidate::Custom(emoji.clone()),
            shortcodes: vec![code],
        }
    }));
    items
}

/// Mart's `SearchIndex.search` with buzz-app's fuzzy shortcode fill. `None`
/// is Mart's "no search", where the categories show.
pub fn search(
    query: &str,
    scope: &str,
    custom: &[CustomEmoji],
    limit: usize,
) -> Option<Vec<Candidate>> {
    if query.trim().is_empty() {
        return None;
    }
    let lower = space_first_hyphen(&query.to_lowercase());
    let mut values: Vec<&str> = Vec::new();
    for word in split_runs(&lower, query_separator) {
        if !word.trim().is_empty() && !values.contains(&word) {
            values.push(word);
        }
    }
    if values.is_empty() {
        return None;
    }
    let items = items(scope, custom);
    let mut scores = vec![0usize; items.len()];
    let mut pool: Vec<usize> = (0..items.len()).collect();
    let mut results: Vec<usize> = Vec::new();
    for value in &values {
        if pool.is_empty() {
            break;
        }
        let needle = format!(",{value}");
        // Each word narrows the pool; only the last word scores.
        results = Vec::new();
        scores.fill(0);
        for &index in &pool {
            let item = &items[index];
            let Some(at) = item.search.find(&needle) else {
                continue;
            };
            results.push(index);
            // `indexOf` counts UTF-16 units.
            scores[index] += if item.id == *value {
                0
            } else {
                item.search[..at].encode_utf16().count() + 1
            };
        }
        pool = results.clone();
    }
    if results.len() >= 2 {
        results.sort_by(|a, b| {
            scores[*a]
                .cmp(&scores[*b])
                .then_with(|| locale_cmp(&items[*a].id, &items[*b].id))
        });
        results.truncate(limit);
    }
    if results.len() < limit {
        let found: HashSet<usize> = results.iter().copied().collect();
        let mut candidates: Vec<(usize, &str)> = items
            .iter()
            .enumerate()
            .filter(|(index, _)| !found.contains(index))
            .flat_map(|(index, item)| item.shortcodes.iter().map(move |code| (index, *code)))
            .collect();
        let needle = super::normalize(&query.replace(':', ""));
        if needle.is_empty() {
            candidates.clear();
        }
        let mut ranked: Vec<((u8, usize), usize, &str)> = candidates
            .into_iter()
            .filter_map(|(index, code)| Some((super::shortcode_score(&needle, code)?, index, code)))
            .collect();
        ranked.sort_by(|(sa, _, a), (sb, _, b)| {
            sa.cmp(sb)
                .then(a.len().cmp(&b.len()))
                .then_with(|| locale_cmp(a, b))
        });
        let mut seen = found;
        for (_, index, _) in ranked {
            if seen.insert(index) {
                results.push(index);
            }
        }
        results.truncate(limit);
    }
    Some(
        results
            .into_iter()
            .map(|index| items[index].candidate.clone())
            .collect(),
    )
}

/// `FrequentlyUsed.DEFAULTS`.
pub const DEFAULTS: [&str; 15] = [
    "+1",
    "grinning",
    "kissing_heart",
    "heart_eyes",
    "laughing",
    "stuck_out_tongue_winking_eye",
    "sweat_smile",
    "joy",
    "scream",
    "disappointed",
    "unamused",
    "weary",
    "sob",
    "sunglasses",
    "heart",
];

/// Mart's pick history (`emoji-mart.frequently` and `emoji-mart.last`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frequent {
    /// `None` until something is saved or the defaults are shown.
    pub index: Option<BTreeMap<String, u64>>,
    pub last: Option<String>,
}

impl Frequent {
    /// `FrequentlyUsed.add`: counts one pick.
    pub fn add(&mut self, id: &str) {
        *self
            .index
            .get_or_insert_default()
            .entry(id.to_string())
            .or_default() += 1;
        self.last = Some(id.to_string());
    }

    /// `FrequentlyUsed.get`: the ids to show, most picked first, at most
    /// `rows` lines. The second value says the history was trimmed and
    /// should be saved.
    pub fn get(&mut self, rows: usize, per_line: usize) -> (Vec<String>, bool) {
        if rows == 0 {
            return (Vec::new(), false);
        }
        let Some(index) = &mut self.index else {
            let defaults: Vec<String> = DEFAULTS
                .iter()
                .take(per_line)
                .map(|id| id.to_string())
                .collect();
            self.index = Some(
                defaults
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (id.clone(), (per_line - i) as u64))
                    .collect(),
            );
            return (defaults, false);
        };
        let max = rows * per_line;
        let mut ids: Vec<String> = index.keys().cloned().collect();
        ids.sort_by(|a, b| index[b].cmp(&index[a]).then_with(|| locale_cmp(a, b)));
        if ids.len() <= max {
            return (ids, false);
        }
        for removed in ids.split_off(max) {
            if Some(&removed) != self.last.as_ref() {
                index.remove(&removed);
            }
        }
        if let Some(last) = &self.last
            && !ids.contains(last)
            && let Some(dropped) = ids.pop()
        {
            index.remove(&dropped);
            ids.push(last.clone());
        }
        (ids, true)
    }
}

/// buzz-app's grid sizes (`EmojiPicker.tsx:33-36`).
const EMOJI_SLOT: f32 = 48.;
const COLUMN_CHROME: f32 = 44.;
const CHROME: f32 = 72.;

/// Emoji per line for the width the picker may use: 1 to 6.
pub fn per_line(available: f32) -> usize {
    ((available - COLUMN_CHROME) / EMOJI_SLOT)
        .floor()
        .clamp(1., 6.) as usize
}

/// The popup's width, borders included.
pub fn width(per_line: usize, available: f32) -> f32 {
    available.min(per_line as f32 * EMOJI_SLOT + CHROME + 2.)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrow {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    /// No grid: the key does what it does in the field.
    Ignore,
    /// Back to the field: clear the selection and let the key through.
    Clear,
    /// Select this (line, column) and keep the key.
    Select(usize, usize),
}

/// Mart's `navigate` over lines of `rows[line]` emoji, from the search
/// field. A key repeat is treated as a fresh press.
pub fn navigate(
    rows: &[usize],
    selected: Option<(usize, usize)>,
    arrow: Arrow,
    caret_at_end: bool,
) -> Nav {
    if rows.is_empty() {
        return Nav::Ignore;
    }
    let last = rows.len() - 1;
    let Some((line, column)) = selected else {
        return if matches!(arrow, Arrow::Right | Arrow::Down) && caret_at_end {
            Nav::Select(0, 0)
        } else {
            Nav::Clear
        };
    };
    if (line, column) == (0, 0) && matches!(arrow, Arrow::Left | Arrow::Up) {
        return Nav::Clear;
    }
    let first = Nav::Select(0, 0);
    let end = Nav::Select(last, rows[last] - 1);
    match arrow {
        Arrow::Left if column > 0 => Nav::Select(line, column - 1),
        Arrow::Left if line == 0 => first,
        Arrow::Left => Nav::Select(line - 1, rows[line - 1] - 1),
        Arrow::Right if column + 1 < rows[line] => Nav::Select(line, column + 1),
        Arrow::Right if line == last => end,
        Arrow::Right => Nav::Select(line + 1, 0),
        Arrow::Up if line == 0 => first,
        Arrow::Up => Nav::Select(line - 1, column.min(rows[line - 1] - 1)),
        Arrow::Down if line == last => end,
        Arrow::Down => Nav::Select(line + 1, column.min(rows[line + 1] - 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn ids(found: &[Candidate]) -> Vec<String> {
        found
            .iter()
            .map(|candidate| match candidate {
                Candidate::Unicode(emoji) => id_of(emoji).unwrap_or("?").to_string(),
                Candidate::Custom(emoji) => format!(":{}:", emoji.shortcode),
            })
            .collect()
    }

    #[test]
    fn maps_every_mart_emoji() {
        assert_eq!(DATA.len(), include_str!("emoji-mart.tsv").lines().count());
        assert_eq!(by_id("+1").unwrap().as_str(), "👍");
        assert_eq!(id_of(emojis::get("😀").unwrap()), Some("grinning"));
    }

    /// `(query, first results, how many)`, from buzz-app's picker search
    /// (Mart's `SearchIndex.search` plus `emoji-mart.ts:566-594`) run on
    /// the same data with the custom emoji `bufo_pray` and `party-parrot`.
    const REFERENCE: &[(&str, &str, usize)] = &[
        (
            "happy",
            "rainbow wink yum grinning joy grin smiley smiley_cat smile joy_cat sweat_smile blush",
            13,
        ),
        (
            "love",
            "love_hotel love_letter heart wedding i_love_you_hand_sign kissing blue_heart \
             two_hearts green_heart heart_hands kiss orange_heart",
            51,
        ),
        ("red heart", "heart", 1),
        (
            "smi",
            "smile smile_cat smiley smiley_cat smiling_face_with_3_hearts smiling_face_with_tear \
             smiling_imp smirk smirk_cat blush relaxed innocent",
            54,
        ),
        (
            "thumbs",
            "-1 +1 hand_with_index_finger_and_thumb_crossed",
            3,
        ),
        ("pointup", "point_up point_up_2 person_in_lotus_position", 3),
        ("openbook", "book", 1),
        ("bfpray", ":bufo_pray:", 1),
        (
            "pray",
            "pray prayer_beads :bufo_pray: man_kneeling kneeling_person woman_kneeling \
             palms_up_together place_of_worship face_with_spiral_eyes suspension_railway \
             person_in_steamy_room person_feeding_baby",
            12,
        ),
        ("parrot", "parrot :party-parrot:", 2),
        (
            "party-p",
            ":party-parrot: partying_face pizza tada balloon crystal_ball mirror_ball \
             confetti_ball beer circus_tent clinking_glasses beers",
            12,
        ),
        ("zzzzqqq", "", 0),
        ("👍", "+1", 1),
        (
            ":smile:",
            "smile smiley smile_cat smiley_cat sweat_smile smiling_face_with_tear \
             smiling_face_with_3_hearts face_with_hand_over_mouth small_airplane \
             kissing_smiling_eyes small_red_triangle small_red_triangle_down",
            14,
        ),
        ("heart eyes", "heart_eyes heart_eyes_cat", 2),
        ("+1", "+1", 1),
    ];

    #[test]
    fn searches_like_the_reference_picker() {
        let custom: Vec<CustomEmoji> = ["bufo_pray", "party-parrot"]
            .into_iter()
            .map(|shortcode| CustomEmoji {
                shortcode: shortcode.into(),
                url: "https://x.test/e.png".into(),
            })
            .collect();
        for (query, first, count) in REFERENCE {
            let found = ids(&search(query, "s", &custom, MAX_RESULTS).unwrap());
            let first: Vec<&str> = first.split_whitespace().collect();
            assert_eq!(&found[..first.len()], first, "{query}");
            assert_eq!(found.len(), *count, "{query}");
        }
        assert_eq!(search("a", "s", &[], MAX_RESULTS).unwrap().len(), 90);
        assert!(search("  ", "s", &[], MAX_RESULTS).is_none());
        assert!(search("|", "s", &[], MAX_RESULTS).is_none());
    }

    #[test]
    fn splits_like_javascript() {
        assert_eq!(split_runs("a--b", |c| c == '-'), ["a", "b"]);
        assert_eq!(split_runs("--a-", |c| c == '-'), ["", "a", ""]);
        assert_eq!(space_first_hyphen("x-ray-y"), "x ray-y");
        assert_eq!(space_first_hyphen("-1"), "-1");
    }

    #[test]
    fn compares_like_locale_compare() {
        assert_eq!(locale_cmp("smile_cat", "smiley"), Ordering::Less);
        assert_eq!(locale_cmp("-1", "+1"), Ordering::Less);
        assert_eq!(locale_cmp("100", "a"), Ordering::Less);
    }

    #[test]
    fn frequent_starts_with_marts_defaults() {
        let mut frequent = Frequent::default();
        let (first, save) = frequent.get(FREQUENT_ROWS, 6);
        assert_eq!(first, DEFAULTS[..6]);
        assert!(!save);
        frequent.add("smile");
        let (ids, _) = frequent.get(FREQUENT_ROWS, 6);
        // The defaults keep their seeded counts; ties sort by id.
        assert_eq!(
            ids,
            [
                "+1",
                "grinning",
                "kissing_heart",
                "heart_eyes",
                "laughing",
                "smile",
                "stuck_out_tongue_winking_eye"
            ]
        );
    }

    #[test]
    fn frequent_keeps_two_lines_and_the_last_pick() {
        let mut frequent = Frequent::default();
        for (i, id) in DEFAULTS.iter().enumerate() {
            for _ in 0..(20 - i) {
                frequent.add(id);
            }
        }
        frequent.add("zap");
        let (ids, save) = frequent.get(FREQUENT_ROWS, 6);
        assert!(save);
        assert_eq!(ids.len(), 12);
        assert_eq!(ids.last().map(String::as_str), Some("zap"));
        assert_eq!(&ids[..11], &DEFAULTS[..11]);
        let index = frequent.index.unwrap();
        assert!(!index.contains_key("weary") && index.contains_key("zap"));
    }

    #[test]
    fn sizes_lines_from_the_width() {
        assert_eq!(per_line(1000.), 6);
        assert_eq!(per_line(44. + 48. * 6.), 6);
        assert_eq!(per_line(44. + 48. * 6. - 1.), 5);
        assert_eq!(per_line(200.), 3);
        assert_eq!(per_line(10.), 1);
        assert_eq!(width(6, 1000.), 362.);
        assert_eq!(width(3, 200.), 200.);
    }

    #[test]
    fn moves_like_marts_navigate() {
        let rows = [3, 3, 1];
        assert_eq!(navigate(&rows, None, Arrow::Down, true), Nav::Select(0, 0));
        assert_eq!(navigate(&rows, None, Arrow::Right, false), Nav::Clear);
        assert_eq!(navigate(&rows, None, Arrow::Left, true), Nav::Clear);
        assert_eq!(navigate(&rows, Some((0, 0)), Arrow::Up, true), Nav::Clear);
        assert_eq!(
            navigate(&rows, Some((0, 2)), Arrow::Right, true),
            Nav::Select(1, 0)
        );
        assert_eq!(
            navigate(&rows, Some((1, 0)), Arrow::Left, true),
            Nav::Select(0, 2)
        );
        assert_eq!(
            navigate(&rows, Some((1, 2)), Arrow::Down, true),
            Nav::Select(2, 0)
        );
        assert_eq!(
            navigate(&rows, Some((2, 0)), Arrow::Down, true),
            Nav::Select(2, 0)
        );
        assert_eq!(
            navigate(&rows, Some((2, 0)), Arrow::Right, true),
            Nav::Select(2, 0)
        );
        assert_eq!(
            navigate(&rows, Some((0, 1)), Arrow::Up, true),
            Nav::Select(0, 0)
        );
        assert_eq!(navigate(&[], None, Arrow::Down, true), Nav::Ignore);
    }
}
