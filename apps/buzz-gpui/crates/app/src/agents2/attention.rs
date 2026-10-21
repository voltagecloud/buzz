//! An agent's attention configuration in the `agent-attention/v1` format
//! (buzz-app's `features/agents2/attention.ts`): Interests, event watches and
//! timers, each one object at a slug. This is the format only (validation,
//! the filter grammar and event matching), so the same objects can later
//! move from local storage to the relay's encrypted addressable events.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const ATTENTION_SCHEMA: &str = "agent-attention/v1";

/// At most this many Interests, and this many watches and timers together.
pub const OBJECT_LIMIT: usize = 100;
const BODY_LIMIT: usize = 65_535;
const TEXT_LIMIT: usize = 16_384;
const MAX_INT: u64 = (1 << 53) - 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interest {
    pub instructions: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClassifierQuestion {
    pub question: String,
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub when_true: Option<String>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub when_false: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    /// The yes probability at which the answer passes;
    /// [`CLASSIFIER_THRESHOLD`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
}

/// Janet's default: a lower threshold passes more events.
#[allow(dead_code)] // Read once a classifier runs here.
pub const CLASSIFIER_THRESHOLD: f64 = 0.2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Classifier {
    pub questions: BTreeMap<String, ClassifierQuestion>,
}

/// Which channels a watch hears: `"all"` or a list of channel ids.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Channels {
    All(AllChannels),
    Some(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AllChannels {
    #[serde(rename = "all")]
    All,
}

#[allow(dead_code)] // Used by agent types and tests.
impl Channels {
    pub const ALL: Channels = Channels::All(AllChannels::All);

    pub fn is_all(&self) -> bool {
        matches!(self, Channels::All(_))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventWatch {
    pub interest_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub enabled: bool,
    pub since: u64,
    pub channels: Channels,
    pub kinds: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<Classifier>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimerWatch {
    pub interest_id: String,
    pub prompt: String,
    pub enabled: bool,
    pub interval_secs: u64,
    pub armed_at: u64,
    pub max_occurrences: Option<u64>,
    pub expires_at: Option<u64>,
}

/// One attention object's value, tagged by `type` as on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AttentionValue {
    Interest(Interest),
    Event(EventWatch),
    Timer(TimerWatch),
}

impl AttentionValue {
    /// The Interest a watch or timer serves.
    pub fn interest_id(&self) -> Option<&str> {
        match self {
            AttentionValue::Interest(_) => None,
            AttentionValue::Event(watch) => Some(&watch.interest_id),
            AttentionValue::Timer(timer) => Some(&timer.interest_id),
        }
    }

    pub fn is_interest(&self) -> bool {
        matches!(self, AttentionValue::Interest(_))
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("attention values serialize")
    }

    /// Validates `value` as the object at `slug` and reads it.
    pub fn parse(slug: &str, value: &Value) -> Result<Self, String> {
        if let Some(problem) = validate_object(slug, value) {
            return Err(problem);
        }
        serde_json::from_value(whole_numbers(value.clone())).map_err(|error| error.to_string())
    }
}

/// `60.0` reads as `60`, as JavaScript numbers do, so validated whole
/// numbers deserialize as integers.
pub fn whole_numbers(value: Value) -> Value {
    match value {
        Value::Number(number) if !number.is_u64() && !number.is_i64() => match number.as_f64() {
            Some(float) if float.fract() == 0.0 && float >= 0.0 && float <= MAX_INT as f64 => {
                Value::from(float as u64)
            }
            _ => Value::Number(number),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(whole_numbers).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, whole_numbers(value)))
                .collect(),
        ),
        other => other,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionObject {
    pub slug: String,
    pub value: AttentionValue,
    /// Unix seconds; on the relay this is the event's `created_at`.
    pub modified_at: u64,
}

/// `interest/<id>` or `watch/<id>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    Interest,
    Watch,
}

fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
}

pub fn parse_slug(slug: &str) -> Option<(Space, &str)> {
    let (space, id) = slug.split_once('/')?;
    let space = match space {
        "interest" => Space::Interest,
        "watch" => Space::Watch,
        _ => return None,
    };
    valid_id(id).then_some((space, id))
}

pub fn interest_slug(id: &str) -> String {
    format!("interest/{id}")
}

#[allow(dead_code)] // For the defaults of agent types.
pub fn watch_slug(id: &str) -> String {
    format!("watch/{id}")
}

type Check<T = ()> = Result<T, String>;

fn fail<T>(message: impl Into<String>) -> Check<T> {
    Err(message.into())
}

fn fields(value: &Map<String, Value>, required: &[&str], optional: &[&str]) -> Check {
    for name in value.keys() {
        if !required.contains(&name.as_str()) && !optional.contains(&name.as_str()) {
            return fail(format!("Unknown field: {name}"));
        }
    }
    for name in required {
        if !value.contains_key(*name) {
            return fail(format!("Missing field: {name}"));
        }
    }
    Ok(())
}

/// `16384` reads as `16,384`, as `toLocaleString("en-US")` does.
pub fn grouped(number: u64) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn text<'a>(value: Option<&'a Value>, name: &str, max: usize) -> Check<&'a str> {
    let Some(Value::String(text)) = value else {
        return fail(format!("{name} must be text"));
    };
    if text.contains('\0') {
        return fail(format!("{name} must be text"));
    }
    if text.len() > max {
        return fail(format!(
            "{name} is longer than {} bytes",
            grouped(max as u64)
        ));
    }
    Ok(text)
}

fn filled<'a>(value: Option<&'a Value>, name: &str, max: usize) -> Check<&'a str> {
    let text = text(value, name, max)?;
    if text.trim().is_empty() {
        return fail(format!("{name} is empty"));
    }
    Ok(text)
}

fn single_line(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c))
}

fn label<'a>(value: Option<&'a Value>, name: &str, max: usize) -> Check<&'a str> {
    let result = text(value, name, max)?;
    if !single_line(result) {
        return fail(format!("{name} must be a single line of text"));
    }
    Ok(result)
}

/// A JSON number that is a whole number (`60` or `60.0`).
fn whole(value: Option<&Value>) -> Option<f64> {
    let number = value?.as_f64()?;
    (number.fract() == 0.0 && number.abs() <= MAX_INT as f64).then_some(number)
}

fn integer(value: Option<&Value>, name: &str, min: u64, max: u64) -> Check<u64> {
    match whole(value) {
        Some(number) if number >= min as f64 && number <= max as f64 => Ok(number as u64),
        _ => fail(format!("{name} must be a whole number from {min} to {max}")),
    }
}

fn unique<T: Ord>(values: impl IntoIterator<Item = T>) -> bool {
    let mut seen = BTreeSet::new();
    values.into_iter().all(|value| seen.insert(value))
}

fn is_channel_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
}

fn interest_id(value: Option<&Value>) -> Check {
    match value {
        Some(Value::String(id)) if valid_id(id) => Ok(()),
        _ => fail("interest_id must be an Interest id"),
    }
}

fn enabled(value: Option<&Value>) -> Check {
    match value {
        Some(Value::Bool(_)) => Ok(()),
        _ => fail("enabled must be true or false"),
    }
}

fn event_watch(value: &Map<String, Value>) -> Check {
    fields(
        value,
        &[
            "type",
            "interest_id",
            "enabled",
            "since",
            "channels",
            "kinds",
        ],
        &["name", "tags", "filter", "classifier"],
    )?;
    interest_id(value.get("interest_id"))?;
    if value.contains_key("name") {
        label(value.get("name"), "name", 256)?;
    }
    enabled(value.get("enabled"))?;
    integer(value.get("since"), "since", 1, MAX_INT)?;
    match value.get("channels") {
        Some(Value::String(all)) if all == "all" => {}
        Some(Value::Array(channels))
            if !channels.is_empty()
                && channels.len() <= 100
                && channels
                    .iter()
                    .all(|c| c.as_str().is_some_and(is_channel_id))
                && unique(channels.iter().filter_map(Value::as_str)) => {}
        _ => return fail("channels must be \"all\" or a list of channel ids"),
    }
    match value.get("kinds") {
        Some(Value::Array(kinds))
            if kinds.len() <= 100
                && kinds.iter().all(|kind| {
                    whole(Some(kind)).is_some_and(|k| (0.0..=65_535.0).contains(&k))
                })
                && unique(kinds.iter().filter_map(Value::as_f64).map(|k| k as i64)) => {}
        _ => return fail("kinds must be a list of event kinds"),
    }
    if let Some(tags) = value.get("tags") {
        let count = tags.as_object().map_or(0, Map::len);
        if count == 0 || count > 16 {
            return fail("tags must name 1 to 16 tags");
        }
        let mut total = 0;
        for (key, values) in tags.as_object().expect("counted above") {
            let mut letters = key.chars();
            let one = matches!((letters.next(), letters.next()), (Some(c), None) if c.is_ascii_alphabetic());
            if !one || key == "h" {
                return fail(format!("Tag {key} must be one letter other than h"));
            }
            let Some(values) = values.as_array().filter(|v| !v.is_empty() && v.len() <= 32) else {
                return fail(format!("Tag {key} needs 1 to 32 values"));
            };
            for item in values {
                let item = label(Some(item), &format!("Tag {key}"), 1_024)?;
                if item.is_empty() {
                    return fail(format!("Tag {key} values are empty"));
                }
                total += item.len();
            }
        }
        if total > TEXT_LIMIT {
            return fail("Tag values are longer than 16,384 bytes");
        }
    }
    if value.contains_key("filter") {
        compile_filter(text(value.get("filter"), "filter", 4_096)?)?;
    }
    if let Some(classifier) = value.get("classifier") {
        let Some(classifier) = classifier.as_object() else {
            return fail("classifier must be an object");
        };
        fields(classifier, &["questions"], &[])?;
        let questions = classifier.get("questions").and_then(Value::as_object);
        let count = questions.map_or(0, Map::len);
        if count == 0 || count > 8 {
            return fail("classifier needs 1 to 8 questions");
        }
        let mut total = 0;
        for (name, question) in questions.expect("counted above") {
            let valid_name = name.len() <= 32
                && name.bytes().enumerate().all(|(i, b)| match i {
                    0 => b.is_ascii_lowercase(),
                    _ => b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_',
                })
                && !name.is_empty();
            if !valid_name || name == "true" || name == "false" {
                return fail(format!("Invalid question name: {name}"));
            }
            let Some(q) = question.as_object() else {
                return fail(format!("Question {name} must be an object"));
            };
            fields(
                q,
                &["question"],
                &["true", "false", "guidance", "threshold"],
            )?;
            for (key, max) in [
                ("question", 2_048),
                ("true", 4_096),
                ("false", 4_096),
                ("guidance", 8_192),
            ] {
                if q.contains_key(key) {
                    total += text(q.get(key), key, max)?.len();
                }
            }
            match q.get("threshold").map(Value::as_f64) {
                None => {}
                Some(Some(threshold)) if threshold > 0.0 && threshold < 1.0 => {}
                _ => return fail("threshold must be between 0 and 1"),
            }
        }
        if total > TEXT_LIMIT {
            return fail("Classifier text is longer than 16,384 bytes");
        }
    }
    Ok(())
}

fn timer(value: &Map<String, Value>) -> Check {
    fields(
        value,
        &[
            "type",
            "interest_id",
            "prompt",
            "enabled",
            "interval_secs",
            "armed_at",
            "max_occurrences",
            "expires_at",
        ],
        &[],
    )?;
    interest_id(value.get("interest_id"))?;
    filled(value.get("prompt"), "prompt", TEXT_LIMIT)?;
    enabled(value.get("enabled"))?;
    integer(value.get("interval_secs"), "interval_secs", 1, 31_536_000)?;
    integer(value.get("armed_at"), "armed_at", 0, MAX_INT)?;
    if value.get("max_occurrences") != Some(&Value::Null) {
        integer(value.get("max_occurrences"), "max_occurrences", 1, MAX_INT)?;
    }
    if value.get("expires_at") != Some(&Value::Null) {
        integer(value.get("expires_at"), "expires_at", 0, MAX_INT)?;
    }
    Ok(())
}

/// Checks one object on its own, as a reader does. Returns a message, or
/// nothing.
pub fn validate_object(slug: &str, value: &Value) -> Option<String> {
    let check = || -> Check {
        let Some((space, _)) = parse_slug(slug) else {
            return fail(format!("Invalid slug: {slug}"));
        };
        let Some(object) = value.as_object() else {
            return fail("value must be an object");
        };
        let kind = object.get("type").and_then(Value::as_str);
        match space {
            Space::Interest => {
                if kind != Some("interest") {
                    return fail("An interest/ value has type \"interest\"");
                }
                fields(object, &["type", "instructions"], &[])?;
                filled(object.get("instructions"), "instructions", TEXT_LIMIT)?;
            }
            Space::Watch => match kind {
                Some("event") => event_watch(object)?,
                Some("timer") => timer(object)?,
                _ => return fail("A watch/ value has type \"event\" or \"timer\""),
            },
        }
        let body = serde_json::json!({ "schema": ATTENTION_SCHEMA, "slug": slug, "value": value });
        if body.to_string().len() > BODY_LIMIT {
            return fail("The object is larger than 65,535 bytes");
        }
        Ok(())
    };
    check().err()
}

/// The parts of an event attention reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventData {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u16,
    pub content: String,
    pub tags: Vec<Vec<String>>,
}

impl EventData {
    pub fn tag(&self, name: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|tag| tag.first().map(String::as_str) == Some(name))
            .and_then(|tag| tag.get(1))
            .map(String::as_str)
    }
}

impl From<&nostr::prelude::Event> for EventData {
    fn from(event: &nostr::prelude::Event) -> Self {
        Self {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: event.created_at.as_secs(),
            kind: event.kind.as_u16(),
            content: event.content.clone(),
            tags: event
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
        }
    }
}

/// A compiled watch filter.
pub type Predicate = Arc<dyn Fn(&EventData) -> bool + Send + Sync>;

const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 256;

/// Compiles the filter grammar:
/// `predicate := or; or := and ("||" and)*; and := atom ("&&" atom)*;
/// atom := "!" atom | "(" predicate ")" | true | false | is_reply
///       | (author | content) "==" string`.
pub fn compile_filter(source: &str) -> Result<Predicate, String> {
    struct Parser<'a> {
        source: &'a str,
        at: usize,
        nodes: usize,
    }
    impl Parser<'_> {
        fn node(&mut self) -> Check {
            self.nodes += 1;
            if self.nodes > MAX_NODES {
                return fail("Filter has more than 256 parts");
            }
            Ok(())
        }
        fn space(&mut self) {
            while self.source[self.at..].starts_with([' ', '\t', '\r', '\n']) {
                self.at += 1;
            }
        }
        fn take(&mut self, token: &str) -> bool {
            self.space();
            if !self.source[self.at..].starts_with(token) {
                return false;
            }
            let end = self.at + token.len();
            // Words must end at a word boundary.
            let word = token.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
            if word
                && self.source[end..]
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return false;
            }
            self.at = end;
            true
        }
        fn string(&mut self) -> Check<String> {
            self.space();
            if !self.source[self.at..].starts_with('"') {
                return fail("Expected a string in filter");
            }
            let bytes = self.source.as_bytes();
            let mut end = self.at + 1;
            while end < bytes.len() && bytes[end] != b'"' {
                end += if bytes[end] == b'\\' { 2 } else { 1 };
            }
            let literal = self.source.get(self.at..(end + 1).min(bytes.len()));
            match literal.and_then(|text| serde_json::from_str::<String>(text).ok()) {
                Some(value) => {
                    self.at = end + 1;
                    Ok(value)
                }
                None => fail("Invalid string in filter"),
            }
        }
        fn atom(&mut self, depth: usize) -> Check<Predicate> {
            if depth > MAX_DEPTH {
                return fail("Filter is nested too deeply");
            }
            self.node()?;
            if self.take("!") {
                let inner = self.atom(depth + 1)?;
                return Ok(Arc::new(move |event| !inner(event)));
            }
            if self.take("(") {
                let inner = self.or(depth + 1)?;
                if !self.take(")") {
                    return fail("Expected ) in filter");
                }
                return Ok(inner);
            }
            if self.take("true") {
                return Ok(Arc::new(|_| true));
            }
            if self.take("false") {
                return Ok(Arc::new(|_| false));
            }
            if self.take("is_reply") {
                return Ok(Arc::new(is_reply));
            }
            for field in ["author", "content"] {
                if self.take(field) {
                    if !self.take("==") {
                        return fail("Expected == in filter");
                    }
                    let expected = self.string()?;
                    return Ok(if field == "author" {
                        Arc::new(move |event| event.pubkey == expected)
                    } else {
                        Arc::new(move |event| event.content == expected)
                    });
                }
            }
            fail("Unexpected token in filter")
        }
        fn and(&mut self, depth: usize) -> Check<Predicate> {
            let mut parts = vec![self.atom(depth)?];
            while self.take("&&") {
                self.node()?;
                parts.push(self.atom(depth)?);
            }
            Ok(if parts.len() == 1 {
                parts.pop().expect("one part")
            } else {
                Arc::new(move |event| parts.iter().all(|part| part(event)))
            })
        }
        fn or(&mut self, depth: usize) -> Check<Predicate> {
            let mut parts = vec![self.and(depth)?];
            while self.take("||") {
                self.node()?;
                parts.push(self.and(depth)?);
            }
            Ok(if parts.len() == 1 {
                parts.pop().expect("one part")
            } else {
                Arc::new(move |event| parts.iter().any(|part| part(event)))
            })
        }
    }
    let mut parser = Parser {
        source,
        at: 0,
        nodes: 0,
    };
    let predicate = parser.or(1)?;
    parser.space();
    if parser.at != source.len() {
        return fail("Unexpected text at the end of the filter");
    }
    Ok(predicate)
}

fn is_reply(event: &EventData) -> bool {
    event.tags.iter().any(|tag| {
        tag.first().map(String::as_str) == Some("e")
            && tag
                .get(3)
                .is_none_or(|marker| ["root", "reply", ""].contains(&marker.as_str()))
    })
}

/// Kinds that are conversation: only these p-tag an agent into a
/// conversation, and only these start a mention run. A reaction to or
/// deletion of the agent's message is addressed (no watch sees it) but is
/// not someone talking to it.
pub const CHAT_KINDS: [u16; 4] = [9, 40003, 40007, 46010];

pub fn tags_agent(event: &EventData, agent: &str) -> bool {
    event.tags.iter().any(|tag| {
        tag.first().map(String::as_str) == Some("p")
            && tag.get(1).map(String::as_str) == Some(agent)
    })
}

/// Directly addressed events take the runtime's built-in mention path,
/// never a watch: a DM to the agent, a chat event that p-tags it, or a
/// non-chat event with an `e` tag naming something it wrote. A chat reply
/// that does not mention it, even in its own thread, is not addressed: only
/// a watch can wake it for one. Its own events are the caller's to skip.
pub fn addressed_to(event: &EventData, agent: &str, wrote: impl Fn(&str) -> bool) -> bool {
    if event.pubkey == agent {
        return false;
    }
    if event.kind == 4 {
        return tags_agent(event, agent);
    }
    if CHAT_KINDS.contains(&event.kind) {
        return tags_agent(event, agent);
    }
    event.tags.iter().any(|tag| {
        tag.first().map(String::as_str) == Some("e")
            && tag.get(1).is_some_and(|id| !id.is_empty() && wrote(id))
    })
}

/// Steps 1, 2 and 4 of event watch matching. Step 3 (addressed events and
/// the agent's own events) belongs to the caller; classifiers are not run
/// here.
pub fn watch_matches(watch: &EventWatch, event: &EventData, filter: Option<&Predicate>) -> bool {
    if !watch.enabled || event.created_at < watch.since {
        return false;
    }
    if let Channels::Some(channels) = &watch.channels {
        match event.tag("h") {
            Some(channel) if channels.iter().any(|c| c == channel) => {}
            _ => return false,
        }
    }
    if !watch.kinds.is_empty() && !watch.kinds.contains(&event.kind) {
        return false;
    }
    for (key, values) in watch.tags.iter().flatten() {
        let found = event.tags.iter().any(|tag| {
            tag.first() == Some(key) && tag.get(1).is_some_and(|value| values.contains(value))
        });
        if !found {
            return false;
        }
    }
    filter.is_none_or(|filter| filter(event))
}

/// Runtime schedule state for one timer. Not config: it never leaves the
/// runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerState {
    pub armed_at: u64,
    pub next_due: u64,
    pub used: u64,
}

/// The saved state while it still describes this schedule. Without one,
/// every occurrence due by `now` counts as used, so a timer never runs more
/// often than it allows (occurrence k is due at armed_at + k × interval).
pub fn timer_state(timer: &TimerWatch, prior: Option<&TimerState>, now: u64) -> TimerState {
    if let Some(prior) = prior
        && prior.armed_at == timer.armed_at
    {
        return *prior;
    }
    let interval = timer.interval_secs.max(1);
    let used = if now < timer.armed_at {
        0
    } else {
        (now - timer.armed_at) / interval
    };
    TimerState {
        armed_at: timer.armed_at,
        next_due: timer.armed_at + (used + 1) * interval,
        used,
    }
}

pub fn timer_spent(timer: &TimerWatch, state: &TimerState, now: u64) -> bool {
    timer.expires_at.is_some_and(|at| now >= at)
        || timer.max_occurrences.is_some_and(|max| state.used >= max)
}

// Reading the format for people: one line per watch, objects grouped by role.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelChoice {
    pub id: String,
    pub name: String,
}

pub const UNITS: [(&str, u64); 4] = [
    ("day", 86_400),
    ("hour", 3_600),
    ("minute", 60),
    ("second", 1),
];

pub fn plural(count: u64, unit: &str) -> String {
    let suffix = if count == 1 {
        ""
    } else if unit.ends_with("ch") || unit.ends_with('s') {
        "es"
    } else {
        "s"
    };
    format!("{count} {unit}{suffix}")
}

/// The largest whole unit of `seconds`: (count, unit, size).
pub fn interval(seconds: u64) -> (u64, &'static str, u64) {
    let (unit, size) = UNITS
        .into_iter()
        .find(|(_, size)| seconds.is_multiple_of(*size))
        .unwrap_or(UNITS[3]);
    (seconds / size, unit, size)
}

pub fn every(seconds: u64) -> String {
    let (count, unit, _) = interval(seconds);
    if count == 1 {
        format!("Every {unit}")
    } else {
        format!("Every {}", plural(count, unit))
    }
}

/// `release-triage` reads as "Release triage".
pub fn interest_title(id: &str) -> String {
    let spaced: String = id
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let words = if spaced.trim().is_empty() {
        id
    } else {
        spaced.trim()
    };
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// One line for a watch or timer, for rows and the peek.
pub fn describe_watch(value: &AttentionValue, channels: &[ChannelChoice]) -> String {
    match value {
        AttentionValue::Timer(timer) => every(timer.interval_secs),
        AttentionValue::Interest(_) => String::new(),
        AttentionValue::Event(watch) => {
            if let Some(name) = &watch.name {
                return name.clone();
            }
            let what = match watch.kinds.as_slice() {
                [9] => "Messages".to_owned(),
                [] => "Any event".to_owned(),
                kinds => format!(
                    "Kind {} events",
                    kinds
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            let place = match &watch.channels {
                Channels::All(_) => "any channel".to_owned(),
                Channels::Some(ids) => match ids.as_slice() {
                    [one] => match channels.iter().find(|choice| &choice.id == one) {
                        Some(choice) => format!("#{}", choice.name),
                        None => plural(1, "channel"),
                    },
                    ids => plural(ids.len() as u64, "channel"),
                },
            };
            let filtered = if watch.filter.is_some() {
                ", filtered"
            } else {
                ""
            };
            format!("{what} in {place}{filtered}")
        }
    }
}

/// The objects by role, in slug order; `orphans` serve no Interest.
pub struct Grouped<'a> {
    pub interests: Vec<&'a AttentionObject>,
    pub watches: Vec<&'a AttentionObject>,
    pub orphans: Vec<&'a AttentionObject>,
}

pub fn attention_of(attention: &BTreeMap<String, AttentionObject>) -> Grouped<'_> {
    // A BTreeMap iterates in slug order already.
    let interests: Vec<_> = attention
        .values()
        .filter(|o| o.value.is_interest())
        .collect();
    let watches: Vec<_> = attention
        .values()
        .filter(|o| !o.value.is_interest())
        .collect();
    let ids: BTreeSet<&str> = interests
        .iter()
        .filter_map(|object| parse_slug(&object.slug).map(|(_, id)| id))
        .collect();
    let orphans = watches
        .iter()
        .copied()
        .filter(|object| {
            object
                .value
                .interest_id()
                .is_none_or(|id| !ids.contains(id))
        })
        .collect();
    Grouped {
        interests,
        watches,
        orphans,
    }
}

#[cfg(test)]
mod tests;
