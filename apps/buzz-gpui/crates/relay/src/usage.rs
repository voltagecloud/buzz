//! Agent turn metrics for one channel: the kind 44200 frames an agent
//! encrypts to its owner, decoded and grouped by session (buzz-app's
//! `features/agents/usage.ts`), and the paged archive scan the Channel
//! usage panel runs over them (`bundled/channel-usage/use-usage-archive.ts`).
//!
//! Counters are what the agent reported. Nothing is priced here: the
//! harness reports `costUsd` itself. A counter nobody reported stays
//! unknown rather than turning into zero.

use crate::archive::{Archive, ArchivePage, ArchiveRecord};
use crate::event::kinds;
use buzz_identity::Identity;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Token and cost counters; `None` means not reported.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counters {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

/// One decoded metrics frame.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRecord {
    pub id: String,
    pub agent: String,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub turn_seq: Option<u64>,
    /// When the agent reported it, in milliseconds.
    pub timestamp: i64,
    pub harness: String,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    /// "authority · model[ · cache class]".
    pub pricing: Option<String>,
    /// This turn's usage; `None` when unreported, unreliable or in conflict.
    pub turn: Option<Counters>,
    /// The session's running totals, only from an identified session with
    /// a turn sequence.
    pub cumulative: Option<Counters>,
    pub delta_reliable: bool,
    /// Another record claims the same turn.
    pub conflict: bool,
}

/// One agent session's records, newest turn first.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageSession {
    pub key: String,
    pub agent: String,
    pub session_id: Option<String>,
    /// The newest trustworthy running totals.
    pub latest: Option<UsageRecord>,
    pub turns: Vec<UsageRecord>,
}

/// The sessions of a channel, and how many of its frames were unreadable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Projection {
    pub groups: Vec<UsageSession>,
    pub unreadable: usize,
}

/// Totals across sessions (`aggregateSessionUsage`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    pub counters: Counters,
    /// Sessions with trustworthy running totals.
    pub complete: usize,
}

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.;
const STOP_REASONS: [&str; 5] = ["end_turn", "max_tokens", "cancelled", "error", "unknown"];

/// A non-empty string of at most 256 UTF-16 units (`label`).
fn label(value: Option<&Value>) -> Option<&str> {
    let text = value?.as_str()?;
    let units = text.encode_utf16().count();
    (units > 0 && units <= 256).then_some(text)
}

/// Present and not `null`: JavaScript's `!= null`.
fn given(value: Option<&Value>) -> bool {
    value.is_some_and(|value| !value.is_null())
}

/// `Number.isSafeInteger` and not negative.
fn safe_count(value: &Value) -> Option<u64> {
    let number = value.as_f64()?;
    (number.is_finite() && number.fract() == 0. && (0. ..=MAX_SAFE_INTEGER).contains(&number))
        .then_some(number as u64)
}

/// The counters of an object; any malformed field rejects them all.
fn counters(value: &Value) -> Option<Counters> {
    let raw = value.as_object()?;
    let count = |key: &str| -> Result<Option<u64>, ()> {
        match raw.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => safe_count(value).map(Some).ok_or(()),
        }
    };
    let cost = match raw.get("costUsd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_f64()
                .filter(|cost| cost.is_finite() && *cost >= 0.)?,
        ),
    };
    Some(Counters {
        input_tokens: count("inputTokens").ok()?,
        output_tokens: count("outputTokens").ok()?,
        total_tokens: count("totalTokens").ok()?,
        cache_read_tokens: count("cacheReadTokens").ok()?,
        cache_write_tokens: count("cacheWriteTokens").ok()?,
        cost_usd: cost,
    })
}

/// `Date.parse` of the ISO forms agents write: RFC 3339, or a bare date
/// (UTC midnight). Milliseconds since the epoch.
fn parse_time(text: &str) -> Option<i64> {
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(time.timestamp_millis());
    }
    let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

fn parse_object(text: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str(text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Decodes a frame's metrics for `channel_id` (`decodeUsage`). The archive
/// already checked the envelope, signature and recipient; this checks
/// only what the metrics mean.
pub fn decode_usage(record: &ArchiveRecord, channel_id: &str) -> Option<UsageRecord> {
    let raw = parse_object(&record.plaintext)?;
    if raw.get("channelId").and_then(Value::as_str) != Some(channel_id) {
        return None;
    }
    let harness = label(raw.get("harness"))?;
    let timestamp = parse_time(raw.get("timestamp")?.as_str()?)?;
    let turn_seq = raw.get("turnSeq");
    if (given(turn_seq) && turn_seq.and_then(safe_count).is_none())
        || (given(raw.get("turnId")) && label(raw.get("turnId")).is_none())
        || (given(raw.get("model")) && label(raw.get("model")).is_none())
        || raw
            .get("deltaReliable")
            .is_some_and(|v| !v.is_null() && !v.is_boolean())
    {
        return None;
    }
    let turn = match raw.get("turn") {
        None | Some(Value::Null) => None,
        Some(value) => Some(counters(value)?),
    };
    let cumulative = match raw.get("cumulative") {
        None | Some(Value::Null) => None,
        Some(value) => Some(counters(value)?),
    };
    let session_id = label(raw.get("sessionId"));
    let turn_seq = turn_seq.and_then(safe_count);
    let pricing = match raw.get("pricingIdentity") {
        None => None,
        Some(value) => {
            let pricing = value.as_object()?;
            let authority = label(pricing.get("authority"))?;
            let model = label(pricing.get("model"))?;
            let cache = match pricing.get("cacheClass") {
                None => None,
                Some(value) => Some(label(Some(value))?),
            };
            Some(match cache {
                Some(cache) => format!("{authority} · {model} · {cache}"),
                None => format!("{authority} · {model}"),
            })
        }
    };
    let stop_reason = match raw.get("stopReason") {
        None | Some(Value::Null) => None,
        Some(Value::String(reason)) if STOP_REASONS.contains(&reason.as_str()) => {
            Some(reason.clone())
        }
        Some(_) => Some("unknown".into()),
    };
    let delta_reliable = raw.get("deltaReliable") != Some(&Value::Bool(false));
    Some(UsageRecord {
        id: record.id.clone(),
        agent: record.agent.clone(),
        session_id: session_id.map(str::to_string),
        turn_id: label(raw.get("turnId")).map(str::to_string),
        turn_seq,
        timestamp,
        harness: harness.to_string(),
        model: label(raw.get("model")).map(str::to_string),
        stop_reason,
        pricing,
        turn: if delta_reliable { turn } else { None },
        cumulative: cumulative.filter(|_| session_id.is_some() && turn_seq.is_some()),
        delta_reliable,
        conflict: false,
    })
}

/// Groups a channel's records into sessions (`projectUsageWithUnreadable`).
/// Duplicate deliveries count once; frames for this channel that do not
/// decode count as unreadable.
pub fn project_usage(records: &[ArchiveRecord], channel_id: &str) -> Projection {
    let mut unreadable = 0;
    let mut order = Vec::new();
    let mut groups: HashMap<String, Vec<UsageRecord>> = HashMap::new();
    let mut seen = HashSet::new();
    for frame in records {
        if !seen.insert(frame.id.as_str()) {
            continue;
        }
        let Some(item) = decode_usage(frame, channel_id) else {
            let ours = parse_object(&frame.plaintext).is_some_and(|raw| {
                raw.get("channelId").and_then(Value::as_str) == Some(channel_id)
            });
            if ours {
                unreadable += 1;
            }
            continue;
        };
        // Missing session identities never collapse into one session.
        let key = serde_json::to_string(&[
            item.agent.as_str(),
            if item.session_id.is_some() {
                "session"
            } else {
                "unidentified"
            },
            item.session_id.as_deref().unwrap_or(&frame.id),
        ])
        .unwrap_or_default();
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(item);
    }
    let mut sessions: Vec<UsageSession> = order
        .into_iter()
        .filter_map(|key| {
            let items = groups.remove(&key)?;
            Some(session(key, items))
        })
        .collect();
    sessions.sort_by(|a, b| {
        let newest = |s: &UsageSession| s.turns.first().map_or(0, |t| t.timestamp);
        newest(b).cmp(&newest(a)).then_with(|| a.key.cmp(&b.key))
    });
    Projection {
        groups: sessions,
        unreadable,
    }
}

fn session(key: String, mut ordered: Vec<UsageRecord>) -> UsageSession {
    let seq = |item: &UsageRecord| item.turn_seq.map_or(-1, |seq| seq as i128);
    ordered.sort_by(|a, b| {
        seq(b)
            .cmp(&seq(a))
            .then_with(|| b.timestamp.cmp(&a.timestamp))
            .then_with(|| b.id.cmp(&a.id))
    });
    // Two records claiming one turn id or sequence are both in conflict.
    let mut conflicts = HashSet::new();
    let mut logical: HashMap<String, &str> = HashMap::new();
    for item in &ordered {
        let identities = item
            .turn_id
            .iter()
            .map(|id| format!("turn:{id}"))
            .chain(item.turn_seq.map(|seq| format!("seq:{seq}")));
        for identity in identities {
            match logical.get(&identity) {
                Some(previous) => {
                    conflicts.insert(previous.to_string());
                    conflicts.insert(item.id.clone());
                }
                None => {
                    logical.insert(identity, &item.id);
                }
            }
        }
    }
    let newest = ordered.iter().find(|item| item.cumulative.is_some());
    let ambiguous = newest.is_some_and(|newest| {
        ordered.iter().any(|item| {
            conflicts.contains(&item.id)
                && matches!((item.turn_seq, newest.turn_seq), (Some(a), Some(b)) if a >= b)
        })
    });
    let first = &ordered[0];
    let latest = newest
        .filter(|_| first.session_id.is_some() && !ambiguous)
        .cloned();
    let (agent, session_id) = (first.agent.clone(), first.session_id.clone());
    let turns = ordered
        .into_iter()
        .map(|item| {
            if conflicts.contains(&item.id) {
                UsageRecord {
                    turn: None,
                    conflict: true,
                    ..item
                }
            } else {
                item
            }
        })
        .collect();
    UsageSession {
        key,
        agent,
        session_id,
        latest,
        turns,
    }
}

/// Sums each session's latest trusted running totals once. A counter is
/// summed only when every session reported it.
pub fn aggregate_session_usage(groups: &[UsageSession]) -> Totals {
    let snapshots: Vec<Option<Counters>> = groups
        .iter()
        .map(|group| group.latest.as_ref().and_then(|latest| latest.cumulative))
        .collect();
    let sum = |field: fn(&Counters) -> Option<u64>| -> Option<u64> {
        if snapshots.is_empty() {
            return None;
        }
        let mut total = 0u64;
        for snapshot in &snapshots {
            total = total.checked_add(field(snapshot.as_ref()?)?)?;
        }
        (total as f64 <= MAX_SAFE_INTEGER).then_some(total)
    };
    let cost = (!snapshots.is_empty())
        .then(|| {
            snapshots
                .iter()
                .map(|snapshot| snapshot.as_ref()?.cost_usd)
                .sum::<Option<f64>>()
        })
        .flatten()
        .filter(|cost| cost.is_finite());
    Totals {
        counters: Counters {
            input_tokens: sum(|c| c.input_tokens),
            output_tokens: sum(|c| c.output_tokens),
            total_tokens: sum(|c| c.total_tokens),
            cache_read_tokens: sum(|c| c.cache_read_tokens),
            cache_write_tokens: sum(|c| c.cache_write_tokens),
            cost_usd: cost,
        },
        complete: snapshots.iter().filter(|s| s.is_some()).count(),
    }
}

/// Records the usage scan reads before it stops.
pub const MAX_RECORDS: usize = 2000;

/// The saved metrics of one viewer and community, as the scan reads them.
pub trait MetricsArchive {
    /// The archive settings' current revision.
    fn revision(&self) -> Result<i64, String>;
    /// One page of saved turn metrics older than `before`.
    fn read(&self, before: Option<i64>) -> Result<ArchivePage, String>;
}

/// The viewer's saved turn metrics in one community of the archive.
#[derive(Clone)]
pub struct SavedMetrics {
    pub archive: Arc<Archive>,
    pub identity: Arc<Identity>,
    /// [`crate::archive::community_key`].
    pub community: String,
}

impl MetricsArchive for SavedMetrics {
    fn revision(&self) -> Result<i64, String> {
        let viewer = self.identity.pubkey_hex();
        Ok(self.archive.settings(&viewer, &self.community)?.revision)
    }

    fn read(&self, before: Option<i64>) -> Result<ArchivePage, String> {
        self.archive.read(
            &self.identity,
            &self.community,
            kinds::AGENT_METRICS,
            None,
            before,
        )
    }
}

/// How a usage scan ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Scan {
    Ready {
        records: Vec<ArchiveRecord>,
        /// Saved records the archive could not open.
        skipped: usize,
        /// The 2,000-record cap stopped it before the oldest record.
        partial: bool,
        revision: i64,
    },
    /// A read failed, or the archive changed under the scan.
    Error,
    /// `live` turned false; nothing should be shown from it.
    Cancelled,
}

/// Reads saved turn metrics newest first, page by page, up to
/// [`MAX_RECORDS`] (the `load` of `useUsageArchive`). Every page must be
/// read under one settings revision, checked before and after it; a
/// change fails the scan rather than mix records. `live` is checked after
/// each step so a superseded scan stops early.
pub fn scan(archive: &impl MetricsArchive, live: impl Fn() -> bool) -> Scan {
    let mut records = Vec::new();
    let (mut scanned, mut skipped) = (0, 0);
    let mut before: Option<i64> = None;
    let mut revision: Option<i64> = None;
    loop {
        let Ok(settings) = archive.revision() else {
            return if live() { Scan::Error } else { Scan::Cancelled };
        };
        if !live() {
            return Scan::Cancelled;
        }
        if revision.is_some_and(|revision| revision != settings) {
            return Scan::Error;
        }
        let Ok(page) = archive.read(before) else {
            return if live() { Scan::Error } else { Scan::Cancelled };
        };
        if !live() {
            return Scan::Cancelled;
        }
        // Mixed revisions fail before the trailing settings read.
        if page.revision != settings || revision.is_some_and(|r| r != page.revision) {
            return Scan::Error;
        }
        let Ok(latest) = archive.revision() else {
            return if live() { Scan::Error } else { Scan::Cancelled };
        };
        if !live() {
            return Scan::Cancelled;
        }
        if page.revision != latest {
            return Scan::Error;
        }
        revision = Some(page.revision);
        scanned += page.records.len() + page.skipped;
        skipped += page.skipped;
        records.extend(page.records);
        records.truncate(MAX_RECORDS);
        // A cursor that does not move back must not loop forever.
        if let (Some(next), Some(current)) = (page.before, before)
            && next >= current
        {
            return Scan::Error;
        }
        if page.before.is_none() || scanned >= MAX_RECORDS {
            return Scan::Ready {
                records,
                skipped,
                partial: page.before.is_some() && scanned >= MAX_RECORDS,
                revision: page.revision,
            };
        }
        before = page.before;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::{Cell, RefCell};

    fn key(n: u64) -> String {
        format!("{n:064x}")
    }

    /// `usage.test.ts`'s frame: goose, timestamp `100 + n` s, channel
    /// "selected".
    fn frame_of(n: u64, payload: Value, owner: &str) -> ArchiveRecord {
        let mut raw = json!({
            "harness": "goose",
            "timestamp": chrono::DateTime::from_timestamp(100 + n as i64, 0).unwrap().to_rfc3339(),
            "channelId": "selected",
        });
        for (k, v) in payload.as_object().unwrap() {
            raw[k] = v.clone();
        }
        ArchiveRecord {
            id: key(n + 10),
            agent: owner.into(),
            created_at: 100,
            received_at: n as i64,
            plaintext: raw.to_string(),
        }
    }

    fn frame(n: u64, payload: Value) -> ArchiveRecord {
        frame_of(n, payload, &key(1))
    }

    #[test]
    fn counts_invalid_matching_rows_once_and_drops_unrelated_and_duplicates() {
        let invalid = frame(1, json!({"timestamp": "invalid"}));
        let unrelated = frame(2, json!({"channelId": "other", "timestamp": "invalid"}));
        let valid = frame(3, json!({"sessionId": "s", "turnSeq": 1}));
        let projection = project_usage(
            &[invalid.clone(), invalid, unrelated, valid.clone()],
            "selected",
        );
        assert_eq!(projection.unreadable, 1);
        assert_eq!(projection.groups.len(), 1);
        assert_eq!(projection.groups[0].session_id.as_deref(), Some("s"));
        assert_eq!(projection.groups[0].turns[0].id, valid.id);
    }

    #[test]
    fn keeps_agents_sessions_and_turns_separate() {
        let mut rows = Vec::new();
        for a in 0..3u64 {
            for t in 0..32u64 {
                rows.push(frame_of(
                    a * 100 + t,
                    json!({
                        "sessionId": format!("session-{}", t / 16),
                        "turnId": format!("turn-{t}"),
                        "turnSeq": t % 16,
                        "turn": {"inputTokens": t, "cacheReadTokens": 0},
                        "cumulative": {"inputTokens": t * 2, "totalTokens": null},
                    }),
                    &key(a + 1),
                ));
            }
        }
        rows.reverse();
        let groups = project_usage(&rows, "selected").groups;
        assert_eq!(groups.len(), 6);
        assert!(groups.iter().all(|g| g.turns.len() == 16));
        assert!(
            groups
                .iter()
                .all(|g| g.latest.as_ref().unwrap().turn_seq == Some(15))
        );
        let latest = groups[0].latest.as_ref().unwrap();
        assert_eq!(latest.cumulative.unwrap().total_tokens, None);
        let last = groups[0].turns.last().unwrap();
        assert_eq!(last.turn.unwrap().cache_read_tokens, Some(0));
    }

    #[test]
    fn uses_only_the_latest_cumulative_snapshot_without_backfilling() {
        let rows = [
            frame(
                1,
                json!({"sessionId": "s", "turnSeq": 1, "cumulative": {"totalTokens": 30, "costUsd": 1}}),
            ),
            frame(
                2,
                json!({"sessionId": "s", "turnSeq": 2, "cumulative": {
                    "inputTokens": 0, "outputTokens": 4, "totalTokens": null, "cacheReadTokens": 0,
                }}),
            ),
            frame(
                3,
                json!({"sessionId": "s", "turnSeq": 3, "turn": {"inputTokens": 3}}),
            ),
        ];
        let groups = project_usage(&rows, "selected").groups;
        assert_eq!(
            groups[0].latest.as_ref().unwrap().cumulative,
            Some(Counters {
                input_tokens: Some(0),
                output_tokens: Some(4),
                cache_read_tokens: Some(0),
                ..Counters::default()
            })
        );
    }

    #[test]
    fn does_not_conflate_unidentified_sessions_duplicates_or_conflicting_turns() {
        let initial = frame(
            1,
            json!({"sessionId": "s", "turnId": "id:initial", "turnSeq": 0}),
        );
        let first = frame(
            2,
            json!({"sessionId": "s", "turnId": "id", "turnSeq": 1, "turn": {"inputTokens": 3}}),
        );
        let conflict = frame(
            3,
            json!({"sessionId": "s", "turnId": "id", "turnSeq": 1, "turn": {"inputTokens": 8}}),
        );
        let groups = project_usage(
            &[
                initial,
                first.clone(),
                conflict,
                first,
                frame(4, json!({"sessionId": null})),
                frame(5, json!({"sessionId": null})),
            ],
            "selected",
        )
        .groups;
        assert_eq!(groups.len(), 3);
        let known = groups
            .iter()
            .find(|g| g.session_id.as_deref() == Some("s"))
            .unwrap();
        assert_eq!(known.turns.len(), 3);
        assert_eq!(known.turns.iter().filter(|t| t.conflict).count(), 2);
        let setup = known
            .turns
            .iter()
            .find(|t| t.turn_id.as_deref() == Some("id:initial"))
            .unwrap();
        assert!(!setup.conflict);

        let reused = project_usage(
            &[
                frame(
                    10,
                    json!({"sessionId": "x", "turnId": "a", "turnSeq": 1, "turn": {"inputTokens": 2}}),
                ),
                frame(
                    11,
                    json!({"sessionId": "x", "turnId": "b", "turnSeq": 1, "turn": {"inputTokens": 3}}),
                ),
            ],
            "selected",
        )
        .groups;
        assert!(
            reused[0]
                .turns
                .iter()
                .all(|t| t.conflict && t.turn.is_none())
        );
    }

    #[test]
    fn does_not_choose_a_cumulative_winner_when_sequences_conflict() {
        let rows = [
            frame(
                30,
                json!({"sessionId": "s", "turnId": "a", "turnSeq": 1, "cumulative": {"totalTokens": 100}}),
            ),
            frame(
                31,
                json!({"sessionId": "s", "turnId": "b", "turnSeq": 2, "cumulative": {"totalTokens": 200}}),
            ),
            frame(
                32,
                json!({"sessionId": "s", "turnId": "c", "turnSeq": 2, "cumulative": {"totalTokens": 999}}),
            ),
        ];
        let group = &project_usage(&rows, "selected").groups[0];
        assert_eq!(group.latest, None);
        assert_eq!(group.turns.iter().filter(|t| t.conflict).count(), 2);
    }

    #[test]
    fn rejects_null_pricing_and_separates_unidentified_from_reported_ids() {
        assert_eq!(
            decode_usage(&frame(20, json!({"pricingIdentity": null})), "selected"),
            None
        );
        let future = decode_usage(
            &frame(23, json!({"stopReason": "future_reason"})),
            "selected",
        );
        assert_eq!(future.unwrap().stop_reason.as_deref(), Some("unknown"));
        let unidentified = frame(21, json!({"sessionId": null}));
        let identified = frame(22, json!({"sessionId": unidentified.id, "turnSeq": 0}));
        assert_eq!(
            project_usage(&[unidentified, identified], "selected")
                .groups
                .len(),
            2
        );
    }

    #[test]
    fn hides_unreliable_turn_usage_and_malformed_or_unrelated_metrics() {
        let bad = frame(6, json!({"turn": {"inputTokens": -1}}));
        let other = frame(7, json!({"channelId": "other"}));
        let channel_less = frame(8, json!({"channelId": null}));
        let unreliable = frame(
            9,
            json!({
                "deltaReliable": false,
                "turn": {"inputTokens": 3},
                "sessionId": "s",
                "turnSeq": 1,
                "cumulative": {"inputTokens": 3},
            }),
        );
        assert_eq!(decode_usage(&bad, "selected"), None);
        let groups = project_usage(&[bad, other, channel_less, unreliable], "selected").groups;
        assert_eq!(groups[0].turns[0].turn, None);
    }

    #[test]
    fn decodes_pricing_cache_counters_and_fractional_integers() {
        let record = decode_usage(
            &frame(
                40,
                json!({
                    "sessionId": "s",
                    "turnSeq": 2.0,
                    "model": "claude",
                    "stopReason": "end_turn",
                    "pricingIdentity": {"authority": "models.dev", "model": "claude", "cacheClass": "5m"},
                    "turn": {"inputTokens": 1200, "outputTokens": 80, "cacheReadTokens": 1000, "cacheWriteTokens": 200, "costUsd": 0.0123},
                    "cumulative": {"totalTokens": 5e3, "costUsd": 0.5},
                }),
            ),
            "selected",
        )
        .unwrap();
        assert_eq!(record.turn_seq, Some(2));
        assert_eq!(record.pricing.as_deref(), Some("models.dev · claude · 5m"));
        let turn = record.turn.unwrap();
        assert_eq!(
            (turn.cache_read_tokens, turn.cache_write_tokens),
            (Some(1000), Some(200))
        );
        assert_eq!(turn.cost_usd, Some(0.0123));
        assert_eq!(record.cumulative.unwrap().total_tokens, Some(5000));
        for bad in [
            json!({"turnSeq": 1.5}),
            json!({"turnSeq": -1}),
            json!({"turnId": ""}),
            json!({"model": 3}),
            json!({"deltaReliable": "yes"}),
            json!({"turn": {"costUsd": -0.1}}),
            json!({"turn": {"inputTokens": 9_007_199_254_740_992u64}}),
            json!({"turn": []}),
            json!({"pricingIdentity": {"authority": "a", "model": "m", "cacheClass": null}}),
            json!({"harness": "x".repeat(257)}),
        ] {
            assert_eq!(
                decode_usage(&frame(41, bad.clone()), "selected"),
                None,
                "{bad}"
            );
        }
        // Running totals need an identified session with a sequence.
        let loose = decode_usage(
            &frame(42, json!({"cumulative": {"totalTokens": 1}})),
            "selected",
        )
        .unwrap();
        assert_eq!(loose.cumulative, None);
    }

    #[test]
    fn totals_use_each_latest_snapshot_once() {
        let groups = project_usage(
            &[
                frame(
                    1,
                    json!({"sessionId": "a", "turnSeq": 1, "cumulative": {"totalTokens": 10, "inputTokens": 8, "costUsd": 0.01}}),
                ),
                frame(
                    2,
                    json!({"sessionId": "a", "turnSeq": 2, "cumulative": {"totalTokens": 15, "inputTokens": 12, "costUsd": 0.02}}),
                ),
                frame(
                    3,
                    json!({"sessionId": "b", "turnSeq": 1, "cumulative": {"totalTokens": 20, "inputTokens": 18, "costUsd": 0}}),
                ),
            ],
            "selected",
        )
        .groups;
        assert_eq!(
            aggregate_session_usage(&groups),
            Totals {
                counters: Counters {
                    total_tokens: Some(35),
                    input_tokens: Some(30),
                    cost_usd: Some(0.02),
                    ..Counters::default()
                },
                complete: 2,
            }
        );
    }

    #[test]
    fn totals_do_not_publish_partial_numbers() {
        let groups = project_usage(
            &[
                frame(
                    1,
                    json!({"sessionId": "a", "turnSeq": 1, "cumulative": {"totalTokens": 10, "inputTokens": 8}}),
                ),
                frame(
                    2,
                    json!({"sessionId": "b", "turnSeq": 1, "cumulative": {"inputTokens": 3}}),
                ),
                frame(
                    3,
                    json!({"sessionId": "c", "turnSeq": 1, "cumulative": {"totalTokens": 5}}),
                ),
                frame(
                    4,
                    json!({"sessionId": "c", "turnSeq": 1, "cumulative": {"totalTokens": 7}}),
                ),
            ],
            "selected",
        )
        .groups;
        assert_eq!(
            aggregate_session_usage(&groups),
            Totals {
                counters: Counters::default(),
                complete: 2
            }
        );
        assert_eq!(aggregate_session_usage(&[]), Totals::default());
    }

    // The scan, as `use-usage-archive.test.tsx` drives it.

    fn metrics(n: u64, channel: &str) -> ArchiveRecord {
        ArchiveRecord {
            id: key(n),
            agent: "1".repeat(64),
            created_at: n,
            received_at: n as i64,
            plaintext: json!({
                "harness": "goose",
                "timestamp": chrono::DateTime::from_timestamp(n as i64, 0).unwrap().to_rfc3339(),
                "sessionId": "s",
                "turnId": n.to_string(),
                "turnSeq": n,
                "channelId": channel,
                "turn": {"inputTokens": n},
            })
            .to_string(),
        }
    }

    fn page(records: Vec<ArchiveRecord>, before: Option<i64>, revision: i64) -> ArchivePage {
        ArchivePage {
            records,
            skipped: 0,
            before,
            revision,
        }
    }

    /// Answers reads from a script; settings answer `revisions` in turn,
    /// then the last one.
    struct Script {
        pages: RefCell<Vec<Result<ArchivePage, String>>>,
        revisions: RefCell<Vec<Result<i64, String>>>,
        reads: RefCell<Vec<Option<i64>>>,
        settings_calls: Cell<usize>,
    }

    impl Script {
        fn new(pages: Vec<Result<ArchivePage, String>>) -> Self {
            Self {
                pages: RefCell::new(pages),
                revisions: RefCell::new(vec![Ok(1)]),
                reads: RefCell::default(),
                settings_calls: Cell::new(0),
            }
        }
    }

    impl MetricsArchive for Script {
        fn revision(&self) -> Result<i64, String> {
            self.settings_calls.set(self.settings_calls.get() + 1);
            let mut revisions = self.revisions.borrow_mut();
            if revisions.len() > 1 {
                revisions.remove(0)
            } else {
                revisions[0].clone()
            }
        }

        fn read(&self, before: Option<i64>) -> Result<ArchivePage, String> {
            self.reads.borrow_mut().push(before);
            self.pages.borrow_mut().remove(0)
        }
    }

    /// `status:groups:partial:skipped:unreadable`, as the test probe shows.
    fn probe(scan: &Scan, channel: &str) -> String {
        match scan {
            Scan::Ready {
                records,
                skipped,
                partial,
                ..
            } => {
                let projection = project_usage(records, channel);
                format!(
                    "ready:{}:{}:{skipped}:{}",
                    projection.groups.len(),
                    if *partial { "partial" } else { "done" },
                    projection.unreadable
                )
            }
            Scan::Error => "error:0:done:0:0".into(),
            Scan::Cancelled => "cancelled".into(),
        }
    }

    #[test]
    fn pages_past_an_unrelated_first_page() {
        let script = Script::new(vec![
            Ok(page(vec![metrics(1, "other")], Some(100), 1)),
            Ok(page(vec![metrics(2, "one"), metrics(2, "one")], None, 1)),
        ]);
        let result = scan(&script, || true);
        assert_eq!(probe(&result, "one"), "ready:1:done:0:0");
        assert_eq!(*script.reads.borrow(), vec![None, Some(100)]);
    }

    #[test]
    fn a_superseded_scan_is_cancelled() {
        let script = Script::new(vec![Ok(page(vec![metrics(1, "one")], None, 1))]);
        assert_eq!(scan(&script, || false), Scan::Cancelled);
    }

    #[test]
    fn read_errors_and_changed_revisions_fail() {
        let failed = Script::new(vec![Err("read failed".into())]);
        assert_eq!(probe(&scan(&failed, || true), "one"), "error:0:done:0:0");
        let changed = Script::new(vec![Ok(page(vec![metrics(2, "one")], None, 2))]);
        assert_eq!(scan(&changed, || true), Scan::Error);
        let retried = Script::new(vec![Ok(page(vec![metrics(3, "one")], None, 1))]);
        assert_eq!(probe(&scan(&retried, || true), "one"), "ready:1:done:0:0");
    }

    #[test]
    fn a_revision_change_after_a_page_fails_before_the_next_read() {
        let script = Script::new(vec![
            Ok(page(vec![metrics(1, "one")], Some(100), 1)),
            Ok(page(Vec::new(), None, 1)),
        ]);
        *script.revisions.borrow_mut() = vec![Ok(1), Ok(1), Ok(2)];
        assert_eq!(scan(&script, || true), Scan::Error);
        assert_eq!(script.reads.borrow().len(), 1);
    }

    #[test]
    fn a_changed_page_revision_fails_without_the_trailing_settings_read() {
        let script = Script::new(vec![
            Ok(page(vec![metrics(1, "one")], Some(100), 1)),
            Ok(page(Vec::new(), None, 2)),
        ]);
        *script.revisions.borrow_mut() = vec![
            Ok(1),
            Ok(1),
            Ok(1),
            Err("trailing settings must not be read".into()),
        ];
        assert_eq!(scan(&script, || true), Scan::Error);
        assert_eq!(script.settings_calls.get(), 3);
    }

    #[test]
    fn skipped_and_invalid_channel_payloads_are_incomplete() {
        let mut invalid = metrics(1, "one");
        invalid.plaintext =
            json!({"channelId": "one", "harness": "goose", "timestamp": "invalid"}).to_string();
        let mut first = page(vec![invalid], None, 1);
        first.skipped = 1;
        let script = Script::new(vec![Ok(first)]);
        assert_eq!(probe(&scan(&script, || true), "one"), "ready:0:done:1:1");
    }

    #[test]
    fn stops_at_the_cap_even_when_pages_are_all_unreadable() {
        let pages = (0..30)
            .map(|n| {
                let mut page = page(Vec::new(), Some(2000 - 100 * (n + 1)), 1);
                page.skipped = 100;
                Ok(page)
            })
            .collect();
        let script = Script::new(pages);
        let result = scan(&script, || true);
        assert_eq!(probe(&result, "one"), "ready:0:partial:2000:0");
        assert_eq!(script.reads.borrow().len(), 20);
    }

    #[test]
    fn a_failed_later_page_is_not_complete() {
        let script = Script::new(vec![
            Ok(page(vec![metrics(1, "one")], Some(100), 1)),
            Err("page failed".into()),
        ]);
        assert_eq!(scan(&script, || true), Scan::Error);
    }

    #[test]
    fn a_cursor_that_does_not_move_back_fails() {
        let script = Script::new(vec![
            Ok(page(vec![metrics(1, "one")], Some(100), 1)),
            Ok(page(vec![metrics(2, "one")], Some(100), 1)),
        ]);
        assert_eq!(scan(&script, || true), Scan::Error);
    }

    #[test]
    fn keeps_at_most_the_cap_of_records() {
        let pages = (0..25)
            .map(|n| {
                let records = (0..100).map(|i| metrics(n * 100 + i + 1, "one")).collect();
                Ok(page(records, Some(10_000 - 100 * (n as i64 + 1)), 1))
            })
            .collect();
        let script = Script::new(pages);
        let Scan::Ready {
            records, partial, ..
        } = scan(&script, || true)
        else {
            panic!("ready");
        };
        assert_eq!((records.len(), partial), (MAX_RECORDS, true));
    }

    /// A throwaway agent's turn metrics, encrypted to `viewer`.
    fn envelope(
        agent: &nostr::prelude::Keys,
        viewer: &Identity,
        frame: &Value,
        at: u64,
    ) -> nostr::prelude::Event {
        use nostr::prelude::{EventBuilder, FinalizeEvent, Kind, Tag, Timestamp, nip44};
        let content = nip44::encrypt(
            agent.secret_key(),
            &viewer.public_key(),
            frame.to_string(),
            nip44::Version::V2,
        )
        .unwrap();
        EventBuilder::new(Kind::from_u16(kinds::AGENT_METRICS), content)
            .tags([
                Tag::parse(["p", &viewer.pubkey_hex()]).unwrap(),
                Tag::parse(["agent", &agent.public_key().to_hex()]).unwrap(),
            ])
            .custom_created_at(Timestamp::from_secs(at))
            .finalize(agent)
            .unwrap()
    }

    #[test]
    fn scans_saved_metrics_from_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Arc::new(Archive::open(dir.path().join("events.sqlite3")).unwrap());
        let alice = Arc::new(Identity::ephemeral());
        let community = "https://relay.example.com".to_string();
        let me = alice.pubkey_hex();
        let revision = archive.settings(&me, &community).unwrap().revision;
        let agent = nostr::prelude::Keys::generate();
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        for seq in 0..3u64 {
            let frame = json!({
                "harness": "goose",
                "model": "claude-sonnet",
                "timestamp": chrono::DateTime::from_timestamp((at - 60 + seq) as i64, 0).unwrap().to_rfc3339(),
                "channelId": "general",
                "sessionId": "session-1",
                "turnId": format!("turn-{seq}"),
                "turnSeq": seq,
                "turn": {"inputTokens": 1000, "outputTokens": 100, "totalTokens": 1100, "cacheReadTokens": 800, "costUsd": 0.004},
                "cumulative": {"inputTokens": 1000 * (seq + 1), "outputTokens": 100 * (seq + 1), "totalTokens": 1100 * (seq + 1), "cacheReadTokens": 800 * (seq + 1), "costUsd": 0.004 * (seq + 1) as f64},
            });
            let event = envelope(&agent, &alice, &frame, at - 60 + seq);
            archive.ingest(&me, &community, &event, revision).unwrap();
        }
        // Another channel's metrics are saved but not shown here.
        let elsewhere =
            json!({"harness": "goose", "timestamp": "2026-10-10T12:00:00Z", "channelId": "random"});
        let event = envelope(&agent, &alice, &elsewhere, at);
        archive.ingest(&me, &community, &event, revision).unwrap();

        let saved = SavedMetrics {
            archive,
            identity: alice,
            community,
        };
        let Scan::Ready {
            records,
            skipped,
            partial,
            ..
        } = scan(&saved, || true)
        else {
            panic!("the scan reads the archive");
        };
        assert_eq!((records.len(), skipped, partial), (4, 0, false));
        let projection = project_usage(&records, "general");
        assert_eq!(projection.unreadable, 0);
        let [session] = projection.groups.as_slice() else {
            panic!("one session");
        };
        assert_eq!(session.agent, agent.public_key().to_hex());
        assert_eq!(session.turns.len(), 3);
        assert_eq!(session.turns[0].turn_seq, Some(2));
        let totals = aggregate_session_usage(&projection.groups);
        assert_eq!(totals.complete, 1);
        assert_eq!(totals.counters.total_tokens, Some(3300));
        assert_eq!(totals.counters.cache_read_tokens, Some(2400));
        assert!((totals.counters.cost_usd.unwrap() - 0.012).abs() < 1e-12);

        // Someone else's key opens none of it.
        let stranger = SavedMetrics {
            identity: Arc::new(Identity::ephemeral()),
            ..saved
        };
        let Scan::Ready { records, .. } = scan(&stranger, || true) else {
            panic!("ready");
        };
        assert!(records.is_empty());
    }
}
