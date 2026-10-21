//! Device-local settings for Agents2 agents (buzz-app's
//! `features/agents2/store.ts`): each agent's attention objects in the
//! `agent-attention/v1` shape, one JSON blob its type owns, and its timers'
//! run state. Which agents exist, their type and name, and their keys live
//! on a host, never here; a record without an identity does nothing.
//!
//! buzz-app keeps this in `localStorage["buzz.agents.v1"]`; here it is a
//! JSON file of the same shape in the data directory.

use super::attention::{
    AttentionObject, AttentionValue, OBJECT_LIMIT, TimerState, TimerWatch, timer_state,
    validate_object,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

pub const STORAGE_KEY: &str = "buzz.agents.v1";
/// Where the records were kept while Agents was called Agents2.
const LEGACY_STORAGE_KEY: &str = "buzz.agents2.v1";

/// Moves the records from their Agents2 file (buzz-app's
/// `migrateAgentStorage`). A new file that already holds something else
/// is left alone, and so is the old one; nothing is merged.
pub fn migrate_storage(dir: &std::path::Path) {
    let old = dir.join(format!("{LEGACY_STORAGE_KEY}.json"));
    let new = dir.join(format!("{STORAGE_KEY}.json"));
    let Ok(value) = std::fs::read(&old) else {
        return;
    };
    match std::fs::read(&new) {
        Ok(current) if current != value => {
            return tracing::error!(
                "Cannot migrate {LEGACY_STORAGE_KEY}: {STORAGE_KEY} already contains different data"
            );
        }
        Ok(_) => {}
        Err(_) => {
            if crate::atomic_file::write(&new, &value).is_err() {
                return tracing::error!("Cannot migrate {LEGACY_STORAGE_KEY}");
            }
        }
    }
    if std::fs::remove_file(&old).is_err() {
        tracing::error!("Cannot migrate {LEGACY_STORAGE_KEY}");
    }
}

/// An object a reader cannot apply, kept as it was stored so it can be
/// fixed.
#[derive(Clone, Debug, PartialEq)]
pub struct SkippedObject {
    pub slug: String,
    pub value: Value,
    pub modified_at: u64,
    pub problem: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct AgentRecord {
    pub pubkey: String,
    /// Keyed by slug (`interest/<id>` or `watch/<id>`). Shared, so a reader
    /// can tell an unchanged set by pointer.
    pub attention: Arc<BTreeMap<String, AttentionObject>>,
    /// Stored objects that are invalid or over a limit. They do nothing, and
    /// stay stored until they are fixed or removed.
    pub skipped: Arc<BTreeMap<String, SkippedObject>>,
    /// Type-owned config. The app never reads it. `Null` when unset.
    pub config: Value,
    /// Run state of its timers, by slug. Not config, so it never leaves the
    /// device; kept here so removing a timer or the agent removes its state.
    pub timers: Arc<BTreeMap<String, TimerState>>,
    /// `Some(true)` when the owner turned attention on. Off, its watches
    /// and timers stay stored but wake nothing, and the agent is not
    /// offered attention tools. Absent means off, so an agent has no
    /// attention until its owner turns it on.
    pub attention_enabled: Option<bool>,
}

impl AgentRecord {
    pub fn blank(pubkey: &str) -> Self {
        Self {
            pubkey: pubkey.into(),
            ..Default::default()
        }
    }

    /// Whether its watches and timers wake it.
    pub fn attention_on(&self) -> bool {
        self.attention_enabled == Some(true)
    }
}

/// How [`set_attention`] treats an edited timer's schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// A new deadline from its `armed_at`, even when that is unchanged.
    Rearm,
    /// A new deadline and a fresh budget, as the owner's "Run again".
    Restart,
}

/// Where the records are kept.
pub trait Storage: Send + Sync {
    fn read(&self) -> Option<String>;
    fn write(&self, text: &str) -> Result<(), String>;
}

/// A JSON file, replaced atomically.
pub struct FileStorage(pub PathBuf);

impl Storage for FileStorage {
    fn read(&self) -> Option<String> {
        std::fs::read_to_string(&self.0).ok()
    }

    fn write(&self, text: &str) -> Result<(), String> {
        crate::atomic_file::write(&self.0, text.as_bytes())
            .map_err(|_| "Agent settings could not be saved".to_string())
    }
}

/// Memory only, for tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStorage(pub std::sync::Mutex<Option<String>>);

#[cfg(test)]
impl Storage for MemoryStorage {
    fn read(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn write(&self, text: &str) -> Result<(), String> {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(text.into());
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredObject {
    slug: String,
    value: Value,
    modified_at: u64,
}

fn is_pubkey(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn group(value: &AttentionValue) -> &'static str {
    if value.is_interest() {
        "Interests"
    } else {
        "watches and timers"
    }
}

fn safe(value: Option<&Value>) -> Option<u64> {
    value?.as_u64().filter(|n| *n < (1 << 53))
}

/// Reads every record. An attention object a reader would not apply is
/// skipped and reported, never dropped.
pub fn read_records(storage: &dyn Storage) -> BTreeMap<String, AgentRecord> {
    let mut records = BTreeMap::new();
    let Some(stored) = storage
        .read()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    else {
        return records;
    };
    if stored.get("version") != Some(&Value::from(1)) {
        return records;
    }
    let Some(agents) = stored.get("agents").and_then(Value::as_object) else {
        return records;
    };
    for (pubkey, record) in agents {
        if !is_pubkey(pubkey) || record.get("pubkey").and_then(Value::as_str) != Some(pubkey) {
            continue;
        }
        let mut attention = BTreeMap::new();
        let mut skipped = BTreeMap::new();
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        let empty = Map::new();
        let mut objects: Vec<_> = record
            .get("attention")
            .and_then(Value::as_object)
            .unwrap_or(&empty)
            .iter()
            .collect();
        // In slug order, so the same objects go over a limit every time.
        objects.sort_by(|a, b| a.0.cmp(b.0));
        for (slug, object) in objects {
            let modified_at = safe(object.get("modifiedAt")).unwrap_or(0);
            let value = object.get("value").cloned().unwrap_or(Value::Null);
            let mut problem = if object.get("slug").and_then(Value::as_str) == Some(slug) {
                validate_object(slug, &value)
            } else {
                Some("Stored under another slug".into())
            };
            let parsed = match &problem {
                None => match serde_json::from_value::<AttentionValue>(
                    super::attention::whole_numbers(value.clone()),
                ) {
                    Ok(parsed) => Some(parsed),
                    Err(error) => {
                        problem = Some(error.to_string());
                        None
                    }
                },
                Some(_) => None,
            };
            if let Some(parsed) = &parsed {
                let kind = group(parsed);
                let count = counts.entry(kind).or_default();
                *count += 1;
                if *count > OBJECT_LIMIT {
                    problem = Some(format!("Over the limit of {OBJECT_LIMIT} {kind}"));
                }
            }
            match (problem, parsed) {
                (None, Some(value)) => {
                    attention.insert(
                        slug.clone(),
                        AttentionObject {
                            slug: slug.clone(),
                            value,
                            modified_at,
                        },
                    );
                }
                (problem, _) => {
                    skipped.insert(
                        slug.clone(),
                        SkippedObject {
                            slug: slug.clone(),
                            value,
                            modified_at,
                            problem: problem.unwrap_or_default(),
                        },
                    );
                }
            }
        }
        let mut timers = BTreeMap::new();
        for (slug, state) in record
            .get("timers")
            .and_then(Value::as_object)
            .unwrap_or(&empty)
        {
            let is_timer = matches!(
                attention
                    .get(slug)
                    .map(|object: &AttentionObject| &object.value),
                Some(AttentionValue::Timer(_))
            );
            let (Some(armed_at), Some(next_due), Some(used)) = (
                safe(state.get("armedAt")),
                safe(state.get("nextDue")),
                safe(state.get("used")),
            ) else {
                continue;
            };
            if is_timer {
                timers.insert(
                    slug.clone(),
                    TimerState {
                        armed_at,
                        next_due,
                        used,
                    },
                );
            }
        }
        records.insert(
            pubkey.clone(),
            AgentRecord {
                pubkey: pubkey.clone(),
                attention: Arc::new(attention),
                skipped: Arc::new(skipped),
                config: record.get("config").cloned().unwrap_or(Value::Null),
                timers: Arc::new(timers),
                attention_enabled: (record.get("attentionEnabled") == Some(&Value::Bool(true)))
                    .then_some(true),
            },
        );
    }
    records
}

/// Writes every record. Skipped objects go back where they were read from,
/// so a later reader that can apply them still finds them.
pub fn write_records(
    storage: &dyn Storage,
    records: &BTreeMap<String, AgentRecord>,
) -> Result<(), String> {
    let mut agents = Map::new();
    for (pubkey, record) in records {
        let mut attention = Map::new();
        for object in record.skipped.values() {
            attention.insert(
                object.slug.clone(),
                serde_json::to_value(StoredObject {
                    slug: object.slug.clone(),
                    value: object.value.clone(),
                    modified_at: object.modified_at,
                })
                .expect("objects serialize"),
            );
        }
        for object in record.attention.values() {
            attention.insert(
                object.slug.clone(),
                serde_json::to_value(object).expect("objects serialize"),
            );
        }
        let mut stored = Map::new();
        stored.insert("pubkey".into(), Value::from(pubkey.clone()));
        stored.insert("attention".into(), Value::Object(attention));
        if !record.config.is_null() {
            stored.insert("config".into(), record.config.clone());
        }
        stored.insert(
            "timers".into(),
            serde_json::to_value(&*record.timers).expect("timers serialize"),
        );
        if let Some(enabled) = record.attention_enabled {
            stored.insert("attentionEnabled".into(), Value::Bool(enabled));
        }
        agents.insert(pubkey.clone(), Value::Object(stored));
    }
    let text = serde_json::json!({ "version": 1, "agents": agents }).to_string();
    storage.write(&text)
}

/// Replaces or deletes (`None`) one attention object, after validating it
/// and the count limits. A timer keeps its run state while its `armed_at`
/// is unchanged, as Janet's do: a new interval applies from its next run,
/// and enabling it again runs at most one overdue occurrence. A new
/// `armed_at` (a rearm) sets a new deadline, one interval after it, and
/// keeps the occurrences already used. Only a timer with no saved state
/// counts what was already due. `schedule` overrides that for a timer.
pub fn set_attention(
    record: &AgentRecord,
    slug: &str,
    value: Option<&Value>,
    now: u64,
    schedule: Option<Schedule>,
) -> Result<AgentRecord, String> {
    let mut rest = (*record.attention).clone();
    let prior = rest.remove(slug).map(|object| object.value);
    let mut skipped = (*record.skipped).clone();
    skipped.remove(slug);
    let mut timers = (*record.timers).clone();
    let state = timers.remove(slug);
    let Some(value) = value else {
        return Ok(AgentRecord {
            attention: Arc::new(rest),
            skipped: Arc::new(skipped),
            timers: Arc::new(timers),
            ..record.clone()
        });
    };
    let parsed = AttentionValue::parse(slug, value)?;
    let kind = group(&parsed);
    if prior.is_none()
        && rest
            .values()
            .filter(|object| group(&object.value) == kind)
            .count()
            >= OBJECT_LIMIT
    {
        return Err(format!("An agent can have at most {OBJECT_LIMIT} {kind}"));
    }
    if let AttentionValue::Timer(timer) = &parsed {
        let kept = match (state, &prior) {
            (Some(state), Some(AttentionValue::Timer(_))) => rearmed(timer, state, schedule),
            _ => timer_state(timer, None, now),
        };
        timers.insert(slug.into(), kept);
    }
    rest.insert(
        slug.into(),
        AttentionObject {
            slug: slug.into(),
            value: parsed,
            modified_at: now,
        },
    );
    Ok(AgentRecord {
        attention: Arc::new(rest),
        skipped: Arc::new(skipped),
        timers: Arc::new(timers),
        ..record.clone()
    })
}

/// `state` for the edited `timer`: unchanged unless it was rearmed.
fn rearmed(timer: &TimerWatch, state: TimerState, schedule: Option<Schedule>) -> TimerState {
    if schedule.is_none() && state.armed_at == timer.armed_at {
        return state;
    }
    TimerState {
        armed_at: timer.armed_at,
        next_due: timer.armed_at + timer.interval_secs,
        used: if schedule == Some(Schedule::Restart) {
            0
        } else {
            state.used
        },
    }
}

/// A group of the owner's agents and the text its members share
/// (buzz-app's `Team`). Membership lives only here: an agent records
/// nothing about its teams.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    pub instructions: String,
    /// Agent pubkeys, in the order shown.
    pub members: Vec<String>,
}

/// buzz-app keeps teams in `localStorage["buzz.agents.teams.v1"]`; here
/// it is a JSON file of the same shape beside the records.
pub const TEAMS_KEY: &str = "buzz.agents.teams.v1";

/// Reads every team, skipping any that is not one.
pub fn read_teams(storage: &dyn Storage) -> Vec<Team> {
    let Some(stored) = storage
        .read()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    else {
        return Vec::new();
    };
    if stored.get("version") != Some(&Value::from(1)) {
        return Vec::new();
    }
    let Some(teams) = stored.get("teams").and_then(Value::as_array) else {
        return Vec::new();
    };
    teams
        .iter()
        .filter_map(|team| {
            let text = |key: &str| team.get(key)?.as_str().map(str::to_owned);
            Some(Team {
                id: text("id")?,
                name: text("name")?,
                instructions: text("instructions")?,
                members: team
                    .get("members")?
                    .as_array()?
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|member| is_pubkey(member))
                    .map(str::to_owned)
                    .collect(),
            })
        })
        .collect()
}

pub fn write_teams(storage: &dyn Storage, teams: &[Team]) -> Result<(), String> {
    let text = serde_json::json!({ "version": 1, "teams": teams }).to_string();
    storage.write(&text)
}

#[cfg(test)]
mod tests;
