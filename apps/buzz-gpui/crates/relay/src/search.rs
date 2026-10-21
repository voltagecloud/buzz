//! Message search (`docs/port/relay.md` §4.2): Slack-style operators, the
//! relay full-text filter and the hits it returns.
//!
//! Pure. The session resolves `from:` names and `in:` channels, runs the
//! read, then re-resolves the hit channels and drops the unreadable ones.
//! Ports `parseSearchOperators.ts` and `useSearchMessages.ts`.

use crate::event::{Verified, kinds};
use crate::socket::Filter;
use nostr::prelude::EventId;
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

/// Kinds a message search returns.
pub const MESSAGE_KINDS: [u16; 3] = [kinds::CHAT, kinds::AGENT_MESSAGE, kinds::DIFF];
/// Hits per search.
pub const LIMIT: usize = 20;
/// Profiles read to resolve a `from:` name.
pub const AUTHOR_LOOKUP_LIMIT: usize = 40;
const PREVIEW_CHARS: usize = 240;

/// Token-start only, not `\b`, which fires after `-` and `/` and would
/// turn `built-in:react` into an operator.
static OPERATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|\s)(from|in|after|before):(\S+)").expect("valid"));
static UNFINISHED_AUTHOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|\s)from:(@?)(\S*)$").expect("valid"));

/// A calendar day, validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Day {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl Day {
    /// `YYYY-MM-DD`, refusing days that do not exist.
    pub fn parse(value: &str) -> Option<Self> {
        let bytes = value.as_bytes();
        if bytes.len() != 10
            || bytes[4] != b'-'
            || bytes[7] != b'-'
            || !value
                .bytes()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
        {
            return None;
        }
        let year: i32 = value[0..4].parse().ok()?;
        let month: u32 = value[5..7].parse().ok()?;
        let day: u32 = value[8..10].parse().ok()?;
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return None,
        };
        (1..=days)
            .contains(&day)
            .then_some(Self { year, month, day })
    }
}

/// A query split into free text and operators. Later operators win;
/// invalid dates stay in the text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedSearch {
    pub text: String,
    /// Raw `from:` value: a key, `@name` or name.
    pub from: Option<String>,
    /// Raw `in:` value: a channel id, `#name` or name.
    pub channel: Option<String>,
    /// `after:` — messages on or after the local start of this day.
    pub after: Option<Day>,
    /// `before:` — messages before the local start of this day.
    pub before: Option<Day>,
}

impl ParsedSearch {
    /// `from:` without a leading `@`.
    pub fn author(&self) -> Option<&str> {
        self.from
            .as_deref()
            .map(|value| value.strip_prefix('@').unwrap_or(value))
            .filter(|value| !value.is_empty())
    }

    /// `in:` without a leading `#`.
    pub fn channel_name(&self) -> Option<&str> {
        self.channel
            .as_deref()
            .map(|value| value.strip_prefix('#').unwrap_or(value))
            .filter(|value| !value.is_empty())
    }

    /// Any operator narrows the search, so it can run without text.
    pub fn has_filters(&self) -> bool {
        self.author().is_some()
            || self.channel_name().is_some()
            || self.after.is_some()
            || self.before.is_some()
    }

    /// The `since`/`until` bounds, given the Unix time of a day's local
    /// midnight. `until` is inclusive in NIP-01, so `before:` stops one
    /// second short of its midnight.
    pub fn range(&self, midnight: impl Fn(Day) -> Option<u64>) -> (Option<u64>, Option<u64>) {
        let since = self.after.and_then(&midnight);
        let until = self
            .before
            .and_then(&midnight)
            .map(|at| at.saturating_sub(1));
        (since, until)
    }
}

/// How well a label matched typed text, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameMatch {
    /// Lower is better: 0 exact, 1 prefix, 2 at a word start, 3 any
    /// substring, 4 letters in order where each run starts at a word start
    /// ("bgp" in "buzz-github-prs"), 5 letters in order.
    pub rank: u8,
    /// The matched code points of the label, for underlining. Empty when
    /// lowercasing changed the label's length (İ → i̇), so no letter is
    /// underlined wrongly.
    pub positions: Vec<usize>,
}

/// How well a page or conversation name matches typed text (`matchName`).
/// `needle` is already lowercased. Case is folded, accents are not, as in
/// buzz-app. Ranks are whole numbers, so callers can add fractional
/// tie-breaks.
pub fn match_name(label: &str, needle: &str) -> Option<NameMatch> {
    let text = label.to_lowercase();
    let chars: Vec<char> = text.chars().collect();
    let aligned = chars.len() == label.chars().count();
    let found = |rank: u8, positions: Vec<usize>| {
        Some(NameMatch {
            rank,
            positions: if aligned { positions } else { Vec::new() },
        })
    };
    // The needle's code points, starting at byte `at` of `text`.
    let run = |at: usize| {
        let from = text[..at].chars().count();
        (from..from + needle.chars().count()).collect::<Vec<_>>()
    };
    if text == needle {
        return found(0, run(0));
    }
    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    let mut substring = None;
    if !needle.is_empty() {
        let mut from = 0;
        while let Some(offset) = text[from..].find(needle) {
            let at = from + offset;
            if at == 0 {
                return found(1, run(0));
            }
            if !word(text[..at].chars().next_back()) {
                return found(2, run(at));
            }
            substring.get_or_insert(at);
            from = at + text[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if let Some(at) = substring {
        return found(3, run(at));
    }
    // Spaces in typed text only separate words; they need not match.
    let letters: Vec<char> = needle.chars().filter(|c| !c.is_whitespace()).collect();
    if letters.is_empty() {
        return None;
    }
    // runs[i][j][in_run]: letters[i..] match chars[j..] with every run of
    // consecutive matches starting at a word start. `in_run` means the
    // previous letter matched chars[j - 1].
    let mut runs = vec![vec![[false; 2]; chars.len() + 1]; letters.len() + 1];
    runs[letters.len()].fill([true; 2]);
    let takes = |runs: &[Vec<[bool; 2]>], i: usize, j: usize, in_run: bool| {
        let starts = in_run || j == 0 || !word(Some(chars[j - 1]));
        chars[j] == letters[i] && starts && runs[i + 1][j + 1][1]
    };
    for i in (0..letters.len()).rev() {
        for j in (0..chars.len()).rev() {
            for in_run in [false, true] {
                runs[i][j][usize::from(in_run)] = takes(&runs, i, j, in_run) || runs[i][j + 1][0];
            }
        }
    }
    if runs[0][0][0] {
        // Replay the same choices: take a letter when it leads to a full
        // match, otherwise skip the character.
        let mut positions = Vec::with_capacity(letters.len());
        let (mut i, mut in_run) = (0, false);
        for j in 0..chars.len() {
            if i == letters.len() {
                break;
            }
            in_run = takes(&runs, i, j, in_run);
            if in_run {
                positions.push(j);
                i += 1;
            }
        }
        return found(4, positions);
    }
    let mut positions = Vec::with_capacity(letters.len());
    for (j, c) in chars.iter().enumerate() {
        if letters.get(positions.len()) == Some(c) {
            positions.push(j);
        }
    }
    if positions.len() == letters.len() {
        found(5, positions)
    } else {
        None
    }
}

/// [`match_name`]'s rank alone (`matchRank`).
pub fn match_rank(label: &str, needle: &str) -> Option<u8> {
    match_name(label, needle).map(|found| found.rank)
}

/// Drops trailing punctuation, so `in:general,` still names `general`.
fn clean(value: &str) -> &str {
    value.trim_end_matches(['.', ',', ';', ':', '!', '?'])
}

/// `parseSearchOperators`.
pub fn parse(raw: &str) -> ParsedSearch {
    let mut parsed = ParsedSearch::default();
    let mut kept = String::new();
    let mut last = 0;
    for captures in OPERATOR.captures_iter(raw) {
        let whole = captures.get(0).expect("match");
        kept.push_str(&raw[last..whole.start()]);
        last = whole.end();
        let value = clean(&captures[2]).to_owned();
        match captures[1].to_ascii_lowercase().as_str() {
            "from" => parsed.from = Some(value),
            "in" => parsed.channel = Some(value),
            "after" => match Day::parse(&value) {
                Some(day) => parsed.after = Some(day),
                None => kept.push_str(whole.as_str()),
            },
            _ => match Day::parse(&value) {
                Some(day) => parsed.before = Some(day),
                None => kept.push_str(whole.as_str()),
            },
        }
    }
    kept.push_str(&raw[last..]);
    parsed.text = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    parsed
}

/// A `from:` still being typed is a person prompt, not a search, unless it
/// is already a key.
pub fn unfinished_author(query: &str) -> Option<String> {
    let captures = UNFINISHED_AUTHOR.captures(query)?;
    let value = captures[2].to_owned();
    (!is_hex_key(&value)).then_some(value)
}

pub fn is_hex_key(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A channel id typed in `in:`.
pub fn is_channel_uuid(value: &str) -> bool {
    crate::discovery::is_uuid(&value.to_ascii_lowercase())
}

/// What the relay is asked: text (prefix mode) and resolved operators.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchRequest {
    pub text: String,
    /// Hex key.
    pub author: Option<String>,
    /// Channel id.
    pub channel: Option<String>,
    pub since: Option<u64>,
    pub until: Option<u64>,
}

impl SearchRequest {
    /// Something to search for.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.author.is_none()
            && self.channel.is_none()
            && self.since.is_none()
            && self.until.is_none()
    }

    /// `{kinds:[9,40002,40008], search, search_mode:"prefix", limit:20,
    /// #h?, authors?, since?, until?}`.
    pub fn filter(&self) -> Filter {
        let mut f = Filter::new();
        f.insert("kinds".into(), json!(MESSAGE_KINDS));
        if !self.text.is_empty() {
            f.insert("search".into(), json!(self.text));
            f.insert("search_mode".into(), json!("prefix"));
        }
        f.insert("limit".into(), json!(LIMIT));
        if let Some(channel) = &self.channel {
            f.insert("#h".into(), json!([channel]));
        }
        if let Some(author) = &self.author {
            f.insert("authors".into(), json!([author]));
        }
        if let Some(since) = self.since {
            f.insert("since".into(), json!(since));
        }
        if let Some(until) = self.until {
            f.insert("until".into(), json!(until));
        }
        f
    }
}

/// The kind 0 prefix lookup behind a typed `from:` name. The relay does
/// not fold accents, so it gets the typed text, lowercased.
pub fn author_lookup_filter(name: &str) -> Filter {
    let mut f = Filter::new();
    f.insert("kinds".into(), json!([kinds::PROFILE]));
    f.insert("search".into(), json!(name.trim().to_lowercase()));
    f.insert("search_mode".into(), json!("prefix"));
    f.insert("limit".into(), json!(AUTHOR_LOOKUP_LIMIT));
    f
}

/// How a `from:` name resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Author {
    Found(String),
    /// Nobody by that name: the search returns nothing.
    Missing,
    /// Several people share the name.
    Ambiguous,
}

/// The one key whose name is exactly `handle` (or that is `handle`).
pub fn resolve_author<'a>(
    handle: &str,
    candidates: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Author {
    let wanted = crate::members::normalize_name(handle);
    if wanted.is_empty() {
        return Author::Missing;
    }
    let mut found: Vec<&str> = candidates
        .into_iter()
        .filter(|(key, name)| crate::members::normalize_name(name) == wanted || *key == wanted)
        .map(|(key, _)| key)
        .collect();
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [] => Author::Missing,
        [one] => Author::Found((*one).to_owned()),
        _ => Author::Ambiguous,
    }
}

/// One message search hit. Relay relevance order is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub id: EventId,
    pub channel: String,
    pub author: String,
    pub created_at: u64,
    /// Whitespace-collapsed text, at most 240 characters.
    pub preview: String,
    /// The owner of trusted workflow output, which shows as "Workflow ·
    /// owned by …" rather than as the relay.
    pub workflow_owner: Option<String>,
}

/// Search returns original indexed events. Hits need one `h`, a message
/// kind, and (when scoped) the scoped channel. `authority` is the relay's
/// own key, which signs workflow output.
pub fn hits(events: &[Verified], scoped: Option<&str>, authority: Option<&str>) -> Vec<SearchHit> {
    events
        .iter()
        .filter_map(|event| {
            if !MESSAGE_KINDS.contains(&event.kind_u16()) {
                return None;
            }
            let mut destinations = event.tags_named("h");
            let channel = destinations.next()?.get(1)?.clone();
            if destinations.next().is_some()
                || channel.is_empty()
                || scoped.is_some_and(|scoped| scoped != channel)
            {
                return None;
            }
            let text = if event.kind_u16() == kinds::AGENT_MESSAGE {
                match serde_json::from_str::<Value>(&event.content) {
                    Ok(Value::Object(body)) => body
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or("Agent message")
                        .to_owned(),
                    _ => "Agent message".to_owned(),
                }
            } else {
                event.content.clone()
            };
            let preview: String = text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(PREVIEW_CHARS)
                .collect();
            Some(SearchHit {
                id: event.id,
                channel,
                author: event.pubkey_hex(),
                created_at: event.created_at_secs(),
                preview: if preview.is_empty() {
                    "Attachment".into()
                } else {
                    preview
                },
                workflow_owner: crate::unread::workflow_owner(event, authority),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{CHANNEL, Signer, T0};

    #[test]
    fn ranks_names_like_buzz_app() {
        assert_eq!(match_rank("General", "general"), Some(0));
        assert_eq!(match_rank("general-chat", "gen"), Some(1));
        assert_eq!(match_rank("team-general", "gen"), Some(2));
        assert_eq!(match_rank("regenerate", "gen"), Some(3));
        assert_eq!(match_rank("buzz-github-prs", "bgp"), Some(4));
        assert_eq!(match_rank("buzz github", "b g"), Some(4));
        assert_eq!(match_rank("Settings", "stg"), Some(5));
        assert_eq!(match_rank("Agents", "xyz"), None);
        assert_eq!(match_rank("Agents", " "), None);
        // Overlapping hits: a later word start still wins over a substring.
        assert_eq!(match_rank("aab ab", "ab"), Some(2));
        // Case only; accents are not folded for names.
        assert_eq!(match_rank("Café", "cafe"), None);
        assert_eq!(match_rank("ÉQUIPE", "équipe"), Some(0));
    }

    fn positions(label: &str, needle: &str) -> Vec<usize> {
        match_name(label, needle).expect("matches").positions
    }

    #[test]
    fn reports_matched_code_points() {
        assert_eq!(positions("General", "general"), [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(positions("team-general", "gen"), [5, 6, 7]);
        assert_eq!(positions("regenerate", "gen"), [2, 3, 4]);
        // Code points, not bytes.
        assert_eq!(positions("ÉÉ général", "gén"), [3, 4, 5]);
        // Each run starts at a word start, as the rank-4 search chose.
        assert_eq!(positions("buzz-github-prs", "bgp"), [0, 5, 12]);
        assert_eq!(positions("buzz-github-prs", "bugi"), [0, 1, 5, 6]);
        assert_eq!(positions("buzz github", "b g"), [0, 5]);
        // Any letters in order.
        assert_eq!(positions("Settings", "stg"), [0, 2, 6]);
        // Lowercasing that changes the length underlines nothing.
        let dotted = match_name("İstanbul", "stan").expect("matches");
        assert_eq!(dotted.rank, 2);
        assert!(dotted.positions.is_empty());
    }

    fn day(year: i32, month: u32, day: u32) -> Day {
        Day { year, month, day }
    }

    #[test]
    fn splits_operators_from_text() {
        let parsed =
            parse("deploy from:@alice in:#general, after:2026-01-02 before:2026-02-01 now");
        assert_eq!(parsed.text, "deploy now");
        assert_eq!(parsed.author(), Some("alice"));
        assert_eq!(parsed.channel_name(), Some("general"));
        assert_eq!(parsed.after, Some(day(2026, 1, 2)));
        assert_eq!(parsed.before, Some(day(2026, 2, 1)));
        assert!(parsed.has_filters());
    }

    #[test]
    fn operators_start_at_a_token_boundary() {
        let parsed = parse("built-in:react https://x.com/in:foo");
        assert_eq!(parsed.channel, None);
        assert_eq!(parsed.text, "built-in:react https://x.com/in:foo");
    }

    #[test]
    fn later_operators_win_and_bad_dates_stay_in_the_text() {
        let parsed = parse("IN:a in:b after:yesterday before:2026-02-30");
        assert_eq!(parsed.channel.as_deref(), Some("b"));
        assert_eq!(parsed.after, None);
        assert_eq!(parsed.before, None);
        assert_eq!(parsed.text, "after:yesterday before:2026-02-30");
        assert_eq!(Day::parse("2024-02-29"), Some(day(2024, 2, 29)));
        assert_eq!(Day::parse("2023-02-29"), None);
    }

    #[test]
    fn before_is_exclusive_of_its_midnight() {
        let parsed = parse("after:2026-01-02 before:2026-01-03");
        let (since, until) = parsed.range(|d| Some(u64::from(d.day) * 100));
        assert_eq!((since, until), (Some(200), Some(299)));
    }

    #[test]
    fn recognizes_an_author_still_being_typed() {
        assert_eq!(unfinished_author("hello from:@ali"), Some("ali".into()));
        assert_eq!(unfinished_author("from:"), Some(String::new()));
        assert_eq!(unfinished_author("from:alice more"), None);
        assert_eq!(unfinished_author(&format!("from:{}", "a".repeat(64))), None);
    }

    #[test]
    fn builds_the_relay_filter() {
        let request = SearchRequest {
            text: "deploy".into(),
            author: Some("ab".into()),
            channel: Some(CHANNEL.into()),
            since: Some(1),
            until: Some(2),
        };
        let f = request.filter();
        assert_eq!(f["kinds"], json!([9, 40002, 40008]));
        assert_eq!(f["search"], json!("deploy"));
        assert_eq!(f["search_mode"], json!("prefix"));
        assert_eq!(f["limit"], json!(20));
        assert_eq!(f["#h"], json!([CHANNEL]));
        assert_eq!(f["authors"], json!(["ab"]));
        let bare = SearchRequest {
            channel: Some(CHANNEL.into()),
            ..Default::default()
        };
        assert!(!bare.filter().contains_key("search"));
        assert!(SearchRequest::default().is_empty());
    }

    #[test]
    fn resolves_authors_by_exact_name() {
        let people = [("k1", "Alice"), ("k2", "Bob"), ("k3", "bob ")];
        assert_eq!(resolve_author("alice", people), Author::Found("k1".into()));
        assert_eq!(resolve_author("Bob", people), Author::Ambiguous);
        assert_eq!(resolve_author("carol", people), Author::Missing);
        assert_eq!(resolve_author("ali", people), Author::Missing);
    }

    #[test]
    fn keeps_single_channel_message_hits() {
        let signer = Signer::new();
        let other = "00000000-0000-4000-8000-000000000002";
        let chat = signer.sign(&signer.viewer, 9, T0, "hello\n  world", &[&["h", CHANNEL]]);
        let agent = signer.sign(
            &signer.viewer,
            40002,
            T0,
            r#"{"content":"from an agent"}"#,
            &[&["h", other]],
        );
        let two = signer.sign(
            &signer.viewer,
            9,
            T0,
            "x",
            &[&["h", CHANNEL], &["h", other]],
        );
        let profile = signer.sign(&signer.viewer, 0, T0, "{}", &[]);
        let empty = signer.sign(&signer.viewer, 9, T0, " ", &[&["h", CHANNEL]]);
        let all = [chat, agent, two, profile, empty];
        let found = hits(&all, None, None);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].preview, "hello world");
        assert_eq!(found[1].preview, "from an agent");
        assert_eq!(found[1].channel, other);
        assert_eq!(found[2].preview, "Attachment");
        assert!(found.iter().all(|hit| hit.workflow_owner.is_none()));
        assert_eq!(hits(&all, Some(other), None).len(), 1);
    }

    #[test]
    fn attributes_workflow_output_to_its_owner() {
        let signer = Signer::new();
        let owner = signer.viewer_hex();
        let workflow = signer.sign(
            &signer.relay,
            9,
            T0,
            "deployed",
            &[
                &["h", CHANNEL],
                &["buzz:workflow", "true"],
                &["buzz:workflow-owner", &owner],
            ],
        );
        let relay = signer.relay_hex();
        let found = hits(std::slice::from_ref(&workflow), None, Some(&relay));
        assert_eq!(found[0].workflow_owner.as_deref(), Some(owner.as_str()));
        // Only the relay's own key can attribute workflow output.
        let found = hits(&[workflow], None, Some(&owner));
        assert_eq!(found[0].workflow_owner, None);
    }
}
