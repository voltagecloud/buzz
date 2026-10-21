//! buzz-app's mention rules (`src/bundled/mentions/README.md`, v2):
//! matching after NFKD and accent folding, the order of choices, when Space
//! selects, and when a multi-word query is still a name. The portable
//! fixtures beside this file are buzz-app's `mention-rules.fixtures.json`
//! (Apache-2.0, © Block, Inc.), unchanged; the tests run them.

use std::collections::HashMap;
use unicode_normalization::UnicodeNormalization;
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

/// The accents that fold away: the "Combining Diacritical Marks" blocks.
fn accent(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0x20d0..=0x20ff | 0xfe20..=0xfe2f)
}

/// One code point folded: NFKD, no accents, lowercase, `σ` for final `ς`.
fn fold_point(c: char) -> String {
    std::iter::once(c)
        .nfkd()
        .filter(|c| !accent(*c))
        .collect::<String>()
        .to_lowercase()
        .replace('ς', "σ")
}

/// A name or query without accents and case, spaces at the ends kept.
pub fn fold_name(text: &str) -> String {
    text.chars().map(fold_point).collect()
}

/// The normalized form for comparing and ordering names.
pub fn normalize_name(text: &str) -> String {
    fold_name(text).trim().to_string()
}

/// Letters, marks and digits make words.
fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| {
        matches!(
            c.general_category_group(),
            GeneralCategoryGroup::Letter
                | GeneralCategoryGroup::Mark
                | GeneralCategoryGroup::Number
        )
    })
}

/// Whether `name` continues `query` with a new word (`Jose Luis` and
/// `Jose-Luis` continue `jose`; `Josefa` does not).
pub fn extends_name(name: &str, query: &str) -> bool {
    let whole: Vec<char> = normalize_name(name).chars().collect();
    let needle: Vec<char> = normalize_name(query).chars().collect();
    !needle.is_empty()
        && whole.len() > needle.len()
        && whole[..needle.len()] == needle[..]
        && !is_word(whole.get(needle.len()).copied())
}

/// The tier at which `name` matches `query`: 0 equal, 1 prefix, 2 a word
/// equals it, 3 a word starts with it. An empty query matches at 0.
pub fn match_tier(name: &str, query: &str) -> Option<u8> {
    match_person(name, query).map(|(tier, _)| tier)
}

/// [`match_tier`] with the char indexes of `name` that matched, for
/// underlining (`matchPerson`). A folded-away accent joins its letter.
pub fn match_person(name: &str, query: &str) -> Option<(u8, Vec<usize>)> {
    let needle: Vec<char> = normalize_name(query).chars().collect();
    let mut chars: Vec<char> = Vec::new();
    let mut origin: Vec<Vec<usize>> = Vec::new();
    for (index, c) in name.chars().enumerate() {
        let pieces: Vec<char> = fold_point(c).chars().collect();
        if pieces.is_empty()
            && let Some(last) = origin.last_mut()
        {
            last.push(index);
        }
        for piece in pieces {
            chars.push(piece);
            origin.push(vec![index]);
        }
    }
    let (mut start, mut end) = (0, chars.len());
    while start < end && chars[start].is_whitespace() {
        start += 1;
    }
    while end > start && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }
    let at =
        |from: usize| from + needle.len() <= end && chars[from..from + needle.len()] == needle[..];
    let found = |tier: u8, from: usize| {
        let mut positions: Vec<usize> = origin[from..from + needle.len()]
            .iter()
            .flatten()
            .copied()
            .collect();
        positions.dedup();
        Some((tier, positions))
    };
    if end - start == needle.len() && at(start) {
        return found(0, start);
    }
    if end - start > needle.len() && at(start) {
        return found(1, start);
    }
    if needle.iter().any(|c| c.is_whitespace()) {
        return None;
    }
    let mut prefix = None;
    for from in start + 1..end {
        if !is_word(Some(chars[from])) || is_word(Some(chars[from - 1])) || !at(from) {
            continue;
        }
        if !is_word(chars.get(from + needle.len()).copied()) {
            return found(2, from);
        }
        prefix.get_or_insert(from);
    }
    prefix.and_then(|from| found(3, from))
}

/// One key the user can mention (`MentionChoice`).
#[derive(Clone, Debug, Default)]
pub struct Choice {
    pub key: String,
    /// The identity's own name.
    pub name: String,
    /// The name shown, possibly qualified.
    pub label: String,
    /// The real names known for the key; none for a key fallback.
    pub aliases: Vec<String>,
    /// The generated key suffix that ends `label` (`Honey · 4prr`), if any.
    pub qualifier: Option<String>,
    pub member: bool,
    pub agent: bool,
    pub owned: bool,
    pub managed: bool,
}

fn name_match(text: &str, query: &str) -> u32 {
    if query.is_empty() {
        0
    } else {
        match_tier(text, query).map_or(u32::MAX, u32::from)
    }
}

fn base_match(choice: &Choice, query: &str) -> u32 {
    let needle = normalize_name(query);
    choice
        .aliases
        .iter()
        .map(|name| name_match(name, &needle))
        .min()
        .unwrap_or(u32::MAX)
}

/// A choice's match: its label's tier, else 4 plus its best alias.
pub fn mention_match(choice: &Choice, query: &str) -> u32 {
    let needle = normalize_name(query);
    if needle.is_empty() {
        return 0;
    }
    if choice.aliases.is_empty() {
        return u32::MAX;
    }
    let visible = name_match(&choice.label, &needle);
    if visible != u32::MAX {
        return visible;
    }
    base_match(choice, query).saturating_add(4)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Online,
    Away,
    Unknown,
}

/// The matching choices in the spec's order (`rankMentions`).
pub fn rank<'a>(
    choices: &'a [Choice],
    query: &str,
    history: &HashMap<String, u64>,
    presence: impl Fn(&str) -> Presence,
) -> Vec<&'a Choice> {
    struct Keyed<'a> {
        choice: &'a Choice,
        tier: [u32; 4],
        label: String,
        block: String,
        recent: u64,
        online: u8,
    }
    let keyed: Vec<Keyed> = choices
        .iter()
        .filter(|c| mention_match(c, query) != u32::MAX)
        .map(|c| {
            let tier = [
                u32::from(!c.member),
                mention_match(c, query),
                base_match(c, query),
                u32::from(!(c.agent && c.owned)),
            ];
            Keyed {
                choice: c,
                tier,
                label: normalize_name(&c.label),
                // People never join a block; on an equal label they sort first.
                block: if c.agent {
                    format!("1{:?}{}", tier, normalize_name(&c.name))
                } else {
                    format!("0{}", c.key)
                },
                recent: history.get(&c.key).copied().unwrap_or(0),
                online: match presence(&c.key) {
                    Presence::Online => 0,
                    Presence::Away => 1,
                    Presence::Unknown => 2,
                },
            }
        })
        .collect();
    let mut first: HashMap<&str, &str> = HashMap::new();
    for k in &keyed {
        let label = first.entry(k.block.as_str()).or_insert(k.label.as_str());
        if k.label.as_str() < *label {
            *label = k.label.as_str();
        }
    }
    // UTF-16 code unit order, as JavaScript compares strings.
    let utf16 = |a: &str, b: &str| a.encode_utf16().cmp(b.encode_utf16());
    let mut order: Vec<usize> = (0..keyed.len()).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (&keyed[a], &keyed[b]);
        a.tier
            .cmp(&b.tier)
            .then_with(|| utf16(first[a.block.as_str()], first[b.block.as_str()]))
            .then_with(|| utf16(&a.block, &b.block))
            .then_with(|| b.recent.cmp(&a.recent))
            .then_with(|| b.choice.managed.cmp(&a.choice.managed))
            .then_with(|| a.online.cmp(&b.online))
            .then_with(|| utf16(&a.label, &b.label))
            .then_with(|| a.choice.key.cmp(&b.choice.key))
    });
    order.into_iter().map(|i| keyed[i].choice).collect()
}

/// The one choice Space selects for `query`, across the full choice set
/// (`exactMention`): an exact name no other name continues.
pub fn exact_mention<'a>(choices: &'a [Choice], query: &str) -> Option<&'a Choice> {
    exact_mention_among(choices, query, |_| true)
}

/// [`exact_mention`] where only choices `among` can equal the name, while
/// every choice can still continue it. A key qualifier or generated
/// `(agent)` suffix only tells namesakes apart, so it never continues the
/// name; a literal name `Honey (agent)` still does.
pub fn exact_mention_among<'a>(
    choices: &'a [Choice],
    query: &str,
    among: impl Fn(&Choice) -> bool,
) -> Option<&'a Choice> {
    let needle = normalize_name(query);
    if needle.is_empty() {
        return None;
    }
    let names = |c: &'a Choice| c.aliases.iter().chain(std::iter::once(&c.label));
    let longer = |c: &'a Choice| -> Vec<&'a str> {
        let label = match c.qualifier.as_deref() {
            Some(qualifier) => c
                .label
                .strip_suffix(&format!(" · {qualifier}"))
                .unwrap_or(&c.label),
            None => &c.label,
        };
        let aliases = c.aliases.iter().map(String::as_str);
        if c.agent
            && c.aliases
                .iter()
                .any(|name| format!("{} (agent)", name.trim()) == label)
        {
            aliases.collect()
        } else {
            aliases.chain(std::iter::once(label)).collect()
        }
    };
    if choices
        .iter()
        .any(|c| longer(c).iter().any(|name| extends_name(name, &needle)))
    {
        return None;
    }
    let mut exact = choices.iter().filter(|c| {
        !c.aliases.is_empty() && among(c) && names(c).any(|name| normalize_name(name) == needle)
    });
    match (exact.next(), exact.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// Whether a multi-word query is still the start of a known name
/// (`matchesMentionQuery`); one word always is.
pub fn admitted(query: &str, names: &[&str]) -> bool {
    if !query.contains(' ') {
        return true;
    }
    let lower = fold_name(query);
    if lower.ends_with(' ')
        && names
            .iter()
            .any(|name| normalize_name(name) == lower.trim_end())
    {
        return false;
    }
    names
        .iter()
        .any(|name| normalize_name(name).starts_with(&lower))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use serde_json::Value;

    fn fixtures() -> Value {
        serde_json::from_str(include_str!("mention-rules.fixtures.json")).unwrap()
    }

    fn choices(case: &Value) -> Vec<Choice> {
        case["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let name = c["name"].as_str().unwrap_or_default().to_string();
                Choice {
                    key: c["pubkey"].as_str().unwrap().into(),
                    label: c["label"]
                        .as_str()
                        .map_or_else(|| name.clone(), str::to_string),
                    aliases: match c.get("aliases") {
                        Some(aliases) => aliases
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|a| a.as_str().unwrap().to_string())
                            .collect(),
                        None => vec![name.clone()],
                    },
                    name,
                    member: c["member"].as_bool().unwrap_or(false),
                    agent: c["agent"].as_bool().unwrap_or(false),
                    owned: c["owned"].as_bool().unwrap_or(false),
                    managed: c["managed"].as_bool().unwrap_or(false),
                    qualifier: c["qualifier"].as_str().map(str::to_string),
                }
            })
            .collect()
    }

    #[test]
    fn matching_fixtures() {
        for case in fixtures()["matching"].as_array().unwrap() {
            let got = match_tier(
                case["label"].as_str().unwrap(),
                case["query"].as_str().unwrap(),
            );
            let expected = case["expected"].as_u64().map(|t| t as u8);
            assert_eq!(got, expected, "{}", case["name"]);
        }
    }

    #[test]
    fn ranking_fixtures() {
        for case in fixtures()["ranking"].as_array().unwrap() {
            let choices = choices(case);
            let history: HashMap<String, u64> = case
                .get("history")
                .and_then(Value::as_object)
                .map(|h| {
                    h.iter()
                        .map(|(k, v)| (k.clone(), v.as_u64().unwrap()))
                        .collect()
                })
                .unwrap_or_default();
            let presence = case.get("presence").cloned().unwrap_or(Value::Null);
            let ranked: Vec<&str> = rank(
                &choices,
                case["query"].as_str().unwrap(),
                &history,
                |key| match presence.get(key).and_then(Value::as_str) {
                    Some("online") => Presence::Online,
                    Some("away") => Presence::Away,
                    _ => Presence::Unknown,
                },
            )
            .into_iter()
            .map(|c| c.key.as_str())
            .collect();
            let expected: Vec<&str> = case["expected"]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| k.as_str().unwrap())
                .collect();
            assert_eq!(ranked, expected, "{}", case["name"]);
        }
    }

    #[test]
    fn space_fixtures() {
        for case in fixtures()["space"].as_array().unwrap() {
            let choices = choices(case);
            let got =
                exact_mention(&choices, case["query"].as_str().unwrap()).map(|c| c.key.as_str());
            assert_eq!(got, case["expected"].as_str(), "{}", case["name"]);
        }
    }

    #[test]
    fn match_positions_cover_letters_and_their_accents() {
        assert_eq!(
            match_person("Mary-Jane", "jane"),
            Some((2, vec![5, 6, 7, 8]))
        );
        assert_eq!(
            match_person("Jose\u{301}", "jose"),
            Some((0, vec![0, 1, 2, 3, 4]))
        );
        assert_eq!(match_person("Honey", "oney"), None);
    }

    #[test]
    fn admission_fixtures() {
        for case in fixtures()["admission"].as_array().unwrap() {
            let names: Vec<&str> = case["names"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_str().unwrap())
                .collect();
            assert_eq!(
                admitted(case["query"].as_str().unwrap(), &names),
                case["expected"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
    }
}
