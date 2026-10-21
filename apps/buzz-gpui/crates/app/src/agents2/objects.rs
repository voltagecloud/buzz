//! One-object reads and writes of an agent's attention (buzz-app's
//! `features/agents/attention-objects.ts`), with Janet's scoped object
//! contract: a show returns the object and its expected-state token, and
//! every write must present the token of the object it replaces. A token
//! is a current-state check, not a credential or a revision history.
//! Storage stays the Agents record; this module only decides what a read
//! shows and whether a write may happen.

use super::attention::{
    AttentionValue, EventWatch, Interest, Space, TimerWatch, interest_slug, parse_slug,
    timer_spent, timer_state, validate_object,
};
use super::store::{AgentRecord, Schedule, set_attention};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fmt;

pub const ABSENT: &str = "object-v1:absent";
/// The most stored attention an agent can write for itself, so that one
/// agent cannot fill the device storage that every agent's settings share.
pub const AGENT_BYTES: usize = 256 * 1024;
/// The shortest interval an agent can give a timer. Each run is a new
/// Claude turn, so a faster timer only queues turns it cannot run.
pub const MIN_INTERVAL_SECS: u64 = 60;

/// Whether a watch's classifier can run on this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassifierAvailability {
    #[allow(dead_code)] // Once a classifier runs here.
    Available,
    Unavailable,
}

/// Whether watch classifiers can run here: the one source. No classifier
/// model runs in this app yet.
pub const CLASSIFIER: ClassifierAvailability = ClassifierAvailability::Unavailable;

/// Enabled/disabled for event watches; armed/inactive/spent for timers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchStatus {
    Enabled,
    Disabled,
    Armed,
    Inactive,
    Spent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    Conflict,
    WrongScope,
    InvalidOperation,
    NotFound,
    Invalid,
}

impl ErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::WrongScope => "wrong-scope",
            Self::InvalidOperation => "invalid-operation",
            Self::NotFound => "not-found",
            Self::Invalid => "invalid",
        }
    }
}

/// A refused attention write; its message starts with the code, as
/// Janet's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionError {
    pub code: ErrorCode,
    pub detail: String,
}

impl AttentionError {
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for AttentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

/// An object as a show presents it: its id beside its fields, and for a
/// watch its status; a timer also has its schedule, the occurrences
/// `used` and when the next is `next_due`.
#[derive(Clone, Debug, PartialEq)]
pub enum ShownObject {
    Interest {
        id: String,
        interest: Interest,
    },
    Event {
        id: String,
        watch: EventWatch,
        status: WatchStatus,
    },
    Timer {
        id: String,
        timer: TimerWatch,
        status: WatchStatus,
        used: u64,
        next_due: u64,
    },
}

impl ShownObject {
    /// The stored value, without the fields a show adds.
    pub fn value(&self) -> AttentionValue {
        match self {
            Self::Interest { interest, .. } => AttentionValue::Interest(interest.clone()),
            Self::Event { watch, .. } => AttentionValue::Event(watch.clone()),
            Self::Timer { timer, .. } => AttentionValue::Timer(timer.clone()),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut object = match self.value().to_json() {
            Value::Object(object) => object,
            _ => Map::new(),
        };
        let (id, status) = match self {
            Self::Interest { id, .. } => (id, None),
            Self::Event { id, status, .. } => (id, Some(status)),
            Self::Timer {
                id,
                status,
                used,
                next_due,
                ..
            } => {
                object.insert("used".into(), json!(used));
                object.insert("next_due".into(), json!(next_due));
                (id, Some(status))
            }
        };
        object.insert("id".into(), json!(id));
        if let Some(status) = status {
            object.insert("status".into(), json!(status));
        }
        Value::Object(object)
    }
}

/// One object and its expected-state token.
#[derive(Clone, Debug, PartialEq)]
pub struct Shown {
    pub object: Option<ShownObject>,
    pub expected_state: String,
}

impl Shown {
    pub fn to_json(&self) -> Value {
        json!({
            "result": if self.object.is_some() { "found" } else { "not-found" },
            "object": self.object.as_ref().map(ShownObject::to_json),
            "expected_state": self.expected_state,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WatchSummary {
    pub id: String,
    pub interest_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub status: WatchStatus,
    /// Whether the watch has a classifier.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub classified: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchFilter {
    pub interest: Option<String>,
    /// `event` or `timer`.
    pub kind: Option<String>,
    pub status: Option<WatchStatus>,
    /// Case-sensitive, over ids and names.
    pub search: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteOptions {
    /// The token from the latest show of this object; [`ABSENT`] to create.
    pub expected: String,
    /// The Interest the caller works for. A watch of another, or another
    /// Interest, is `wrong-scope`.
    pub interest: Option<String>,
    /// A timer rearm: its deadline is set from `armed_at` even when that
    /// is unchanged, keeping the occurrences used.
    pub rearm: bool,
}

fn timer_object(record: &AgentRecord, id: &str, timer: &TimerWatch, now: u64) -> ShownObject {
    let state = timer_state(
        timer,
        record.timers.get(&super::attention::watch_slug(id)),
        now,
    );
    let status = if !timer.enabled {
        WatchStatus::Inactive
    } else if timer_spent(timer, &state, now) {
        WatchStatus::Spent
    } else {
        WatchStatus::Armed
    };
    ShownObject::Timer {
        id: id.into(),
        timer: timer.clone(),
        status,
        used: state.used,
        next_due: state.next_due,
    }
}

/// The object at `slug` as a show presents it, or `None`.
pub fn object_at(record: &AgentRecord, slug: &str, now: u64) -> Option<ShownObject> {
    let (_, id) = parse_slug(slug)?;
    Some(match &record.attention.get(slug)?.value {
        AttentionValue::Interest(interest) => ShownObject::Interest {
            id: id.into(),
            interest: interest.clone(),
        },
        AttentionValue::Event(watch) => ShownObject::Event {
            id: id.into(),
            watch: watch.clone(),
            status: if watch.enabled {
                WatchStatus::Enabled
            } else {
                WatchStatus::Disabled
            },
        },
        AttentionValue::Timer(timer) => timer_object(record, id, timer, now),
    })
}

/// `object-v1:` and the SHA-256 of the canonical object (keys sorted), or
/// [`ABSENT`].
pub fn token(object: Option<&ShownObject>) -> String {
    let Some(object) = object else {
        return ABSENT.into();
    };
    // serde_json's maps are sorted, so this is the canonical form.
    let text = object.to_json().to_string();
    let digest = Sha256::digest(text.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("object-v1:{hex}")
}

/// Refuses an object outside the caller's Interest, as Janet's scope does.
fn check_scope(
    record: &AgentRecord,
    slug: &str,
    value: Option<&AttentionValue>,
    interest: Option<&str>,
) -> Result<(), AttentionError> {
    let (Some(interest), Some((space, id))) = (interest, parse_slug(slug)) else {
        return Ok(());
    };
    let prior = record.attention.get(slug).map(|object| &object.value);
    let owner = match space {
        Space::Interest => id,
        Space::Watch => prior
            .and_then(AttentionValue::interest_id)
            .or_else(|| value.and_then(AttentionValue::interest_id))
            .unwrap_or(interest),
    };
    if owner == interest {
        return Ok(());
    }
    Err(AttentionError::new(
        ErrorCode::WrongScope,
        match space {
            Space::Interest => format!("Interest {id} is not {interest}"),
            Space::Watch => format!("Watch {id} belongs to Interest {owner}"),
        },
    ))
}

pub fn show(
    record: &AgentRecord,
    slug: &str,
    now: u64,
    interest: Option<&str>,
) -> Result<Shown, AttentionError> {
    check_scope(record, slug, None, interest)?;
    let object = object_at(record, slug, now);
    Ok(Shown {
        expected_state: token(object.as_ref()),
        object,
    })
}

/// Interest ids, in order; `search` is case-sensitive.
pub fn interest_ids(record: &AgentRecord, search: &str) -> Vec<String> {
    let mut ids: Vec<String> = record
        .attention
        .values()
        .filter(|object| object.value.is_interest())
        .filter_map(|object| parse_slug(&object.slug).map(|(_, id)| id.to_owned()))
        .filter(|id| id.contains(search))
        .collect();
    ids.sort();
    ids
}

pub fn watch_summaries(record: &AgentRecord, filter: &WatchFilter, now: u64) -> Vec<WatchSummary> {
    let search = filter.search.as_deref().unwrap_or("");
    let mut summaries: Vec<WatchSummary> = record
        .attention
        .values()
        .filter_map(|object| {
            let (_, id) = parse_slug(&object.slug)?;
            Some(match &object.value {
                AttentionValue::Interest(_) => return None,
                AttentionValue::Event(watch) => WatchSummary {
                    id: id.into(),
                    interest_id: watch.interest_id.clone(),
                    name: watch.name.clone().filter(|name| !name.is_empty()),
                    kind: "event",
                    status: if watch.enabled {
                        WatchStatus::Enabled
                    } else {
                        WatchStatus::Disabled
                    },
                    classified: watch.classifier.is_some(),
                },
                AttentionValue::Timer(timer) => WatchSummary {
                    id: id.into(),
                    interest_id: timer.interest_id.clone(),
                    name: None,
                    kind: "timer",
                    status: match timer_object(record, id, timer, now) {
                        ShownObject::Timer { status, .. } => status,
                        _ => WatchStatus::Armed,
                    },
                    classified: false,
                },
            })
        })
        .filter(|watch| {
            filter
                .interest
                .as_ref()
                .is_none_or(|interest| *interest == watch.interest_id)
                && filter.kind.as_deref().is_none_or(|kind| kind == watch.kind)
                && filter.status.is_none_or(|status| status == watch.status)
                && (watch.id.contains(search)
                    || watch
                        .name
                        .as_ref()
                        .is_some_and(|name| name.contains(search)))
        })
        .collect();
    summaries.sort_by(|a, b| a.id.cmp(&b.id));
    summaries
}

/// The record after one checked write: replaces (or with `None` removes)
/// the object at `slug`. Refused writes are an [`AttentionError`].
pub fn checked_write(
    record: &AgentRecord,
    slug: &str,
    value: Option<&Value>,
    options: &WriteOptions,
    now: u64,
) -> Result<AgentRecord, AttentionError> {
    use ErrorCode::*;
    let fail = |code, detail: String| Err(AttentionError::new(code, detail));
    let Some((space, id)) = parse_slug(slug) else {
        return fail(Invalid, format!("Invalid slug: {slug}"));
    };
    if !options.expected.starts_with("object-v1:") {
        return fail(
            InvalidOperation,
            "Every write needs the expected_state of a show".into(),
        );
    }
    let value = match value {
        None => None,
        Some(value) => {
            if let Some(problem) = validate_object(slug, value) {
                return fail(Invalid, problem);
            }
            Some(AttentionValue::parse(slug, value).map_err(|e| AttentionError::new(Invalid, e))?)
        }
    };
    let prior = record.attention.get(slug).map(|object| &object.value);
    // An interval the owner set stays editable; the agent cannot set a new
    // one.
    if let Some(AttentionValue::Timer(timer)) = &value
        && timer.interval_secs < MIN_INTERVAL_SECS
        && !matches!(prior, Some(AttentionValue::Timer(was)) if was.interval_secs == timer.interval_secs)
    {
        return fail(
            Invalid,
            format!("interval_secs must be at least {MIN_INTERVAL_SECS}"),
        );
    }
    // Stored by a newer app, or invalid: this app cannot show it, so it
    // cannot check a token against it, and a write would replace what it
    // never read.
    if record.skipped.contains_key(slug) {
        return fail(
            InvalidOperation,
            format!(
                "{slug} is stored in a form this app cannot read; your owner can fix or remove it"
            ),
        );
    }
    check_scope(record, slug, value.as_ref(), options.interest.as_deref())?;
    if token(object_at(record, slug, now).as_ref()) != options.expected {
        return fail(
            Conflict,
            if prior.is_some() {
                format!("{slug} changed since it was read; show it again")
            } else {
                format!("{slug} does not exist; show it again")
            },
        );
    }
    if let Some(value) = &value
        && value.is_interest() != (space == Space::Interest)
    {
        return fail(
            Invalid,
            format!(
                "{slug} cannot hold a {}",
                if value.is_interest() {
                    "Interest"
                } else {
                    "watch"
                }
            ),
        );
    }
    if let (Some(prior), Some(value)) = (prior, &value)
        && !prior.is_interest()
        && !value.is_interest()
    {
        if std::mem::discriminant(prior) != std::mem::discriminant(value) {
            return fail(
                InvalidOperation,
                format!(
                    "Watch {id} is {}; its type cannot change",
                    if matches!(prior, AttentionValue::Event(_)) {
                        "an event watch"
                    } else {
                        "a timer"
                    }
                ),
            );
        }
        if prior.interest_id() != value.interest_id() {
            return fail(
                InvalidOperation,
                "A watch cannot move to another Interest".into(),
            );
        }
    }
    if let Some(interest) = value.as_ref().and_then(AttentionValue::interest_id)
        && !record.attention.contains_key(&interest_slug(interest))
    {
        return fail(
            NotFound,
            format!("Interest {interest} does not exist; set it first"),
        );
    }
    if value.is_none() && space == Space::Interest {
        let filter = WatchFilter {
            interest: Some(id.into()),
            ..Default::default()
        };
        let users = watch_summaries(record, &filter, now);
        if !users.is_empty() {
            let ids: Vec<&str> = users.iter().map(|watch| watch.id.as_str()).collect();
            return fail(
                InvalidOperation,
                format!("Remove its watches first: {}", ids.join(", ")),
            );
        }
    }
    let json = value.as_ref().map(AttentionValue::to_json);
    let schedule = options.rearm.then_some(Schedule::Rearm);
    let after = set_attention(record, slug, json.as_ref(), now, schedule)
        .map_err(|error| AttentionError::new(Invalid, error))?;
    if value.is_some() && stored_bytes(&after) > AGENT_BYTES {
        return fail(
            Invalid,
            format!(
                "Your attention would take more than {} KB; remove or shorten other objects first",
                AGENT_BYTES / 1024
            ),
        );
    }
    Ok(after)
}

/// The bytes the record's attention takes in storage, unreadable objects
/// too.
fn stored_bytes(record: &AgentRecord) -> usize {
    let skipped: Map<String, Value> = record
        .skipped
        .values()
        .map(|object| {
            (
                object.slug.clone(),
                json!({
                    "slug": object.slug,
                    "value": object.value,
                    "modifiedAt": object.modified_at,
                    "problem": object.problem,
                }),
            )
        })
        .collect();
    json!([&*record.attention, skipped]).to_string().len()
}

#[cfg(test)]
mod tests;
