//! Agent activity (`features/agents/activity.ts`): the owner-only telemetry
//! the viewer's agents send as encrypted kind 24200 frames, decrypted with
//! the viewer's key, kept in a bounded live window beside one page of the
//! local archive, and folded into turn states and transcripts.
//!
//! [`Activity`] is the state, free of GPUI so it can be tested; the
//! [`observer`] module runs it for a community. Plaintext never leaves
//! memory: the archive keeps only the encrypted envelopes.

pub mod current;
pub mod local;
pub mod observer;
pub mod request;
pub mod transcript_model;
pub mod turns;

use local::LocalWork;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use turns::{ActivityTurn, TriggerMessage, Turns, Typing};

/// Frames older than this leave the display (`ACTIVITY_HISTORY_AGE_MS`).
pub const HISTORY_AGE_MS: u64 = 90 * 24 * 60 * 60 * 1000;
/// The live window keeps at most this many unsaved records...
pub const RECORD_LIMIT: usize = 200;
/// ...and this many bytes of their plaintext.
pub const BYTE_LIMIT: usize = 2 * 1024 * 1024;
/// A decrypted frame's largest plaintext (`OBSERVER_PLAINTEXT_BYTES`).
pub const PLAINTEXT_BYTES: usize = 65_535;

/// A decrypted observer frame (`ObserverFrame`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub id: String,
    pub agent: String,
    pub created_at: u64,
    pub plaintext: String,
}

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Frame {
    /// Checks a frame the host decoded (`observerFrame`): lowercase hex
    /// ids, bounded JSON plaintext. Unknown fields stay inert.
    pub fn parse(value: &Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?;
        let agent = value.get("agent")?.as_str()?;
        let created_at = value.get("createdAt")?.as_u64()?;
        let plaintext = value.get("plaintext")?.as_str()?;
        if !hex64(id)
            || !hex64(agent)
            || created_at > 9_007_199_254_740_991
            || plaintext.len() > PLAINTEXT_BYTES
            || serde_json::from_str::<Value>(plaintext).is_err()
        {
            return None;
        }
        Some(Self {
            id: id.into(),
            agent: agent.into(),
            created_at,
            plaintext: plaintext.into(),
        })
    }
}

/// A frame in the display: live, or from a saved page.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub id: String,
    pub agent: String,
    pub created_at: u64,
    pub plaintext: Arc<str>,
    /// Milliseconds.
    pub received_at: u64,
    pub historical: bool,
    /// The envelope's `kind`, or "unknown".
    pub kind: String,
    /// The channels the envelope and its batch children name.
    pub channel_ids: Vec<String>,
}

/// Whether the live feed is receiving (`Snapshot.status`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FeedStatus {
    /// This host cannot decode activity.
    Unavailable,
    /// Nothing holds the feed.
    #[default]
    Disabled,
    Connecting,
    Listening,
    Interrupted,
}

impl FeedStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Disabled => "disabled",
            Self::Connecting => "connecting",
            Self::Listening => "listening",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryStatus {
    #[default]
    Unavailable,
    Loading,
    Ready,
    Error,
}

/// Whether the archive saves new frames (`Snapshot.capture`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Capture {
    #[default]
    Unknown,
    Saving,
    Off,
    Error,
}

/// What views read; replaced as a whole when anything changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub status: FeedStatus,
    pub records: Arc<[Arc<Record>]>,
    pub turns: Arc<[ActivityTurn]>,
    /// This app's own agents' requests in this community, in channels the
    /// viewer can read ([`local`]).
    pub local: Arc<[LocalWork]>,
    /// Working typing evidence only.
    pub typing: Arc<[Typing]>,
    /// Records and turn states that left the live window.
    pub trimmed: usize,
    pub history: HistoryStatus,
    pub has_older: bool,
    /// A page other than the newest is shown.
    pub history_older: bool,
    pub history_skipped: usize,
    pub history_agents: Arc<[String]>,
    pub capture: Capture,
    /// Channels with working evidence, sorted.
    pub working_channels: Arc<[String]>,
    /// `(channel, message)` pairs live turn starts name as their triggers:
    /// the messages telemetry may tie work to.
    pub requested: Arc<[(String, String)]>,
}

/// One page read from the archive, decrypted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    /// Newest first, with when each arrived (ms).
    pub records: Vec<(Frame, u64)>,
    pub skipped: usize,
    pub agents: Vec<String>,
    /// The cursor of the next older page.
    pub before: Option<i64>,
}

impl Page {
    /// The JSON `Identity::decode_archive_page` returns.
    pub fn parse(value: &Value) -> Option<Self> {
        let records = value
            .get("records")?
            .as_array()?
            .iter()
            .filter_map(|record| Some((Frame::parse(record)?, record.get("receivedAt")?.as_u64()?)))
            .collect();
        Some(Self {
            records,
            skipped: value.get("skipped").and_then(Value::as_u64).unwrap_or(0) as usize,
            agents: value
                .get("agents")
                .and_then(Value::as_array)
                .map(|agents| {
                    agents
                        .iter()
                        .filter_map(|agent| agent.as_str().filter(|a| hex64(a)).map(Into::into))
                        .collect()
                })
                .unwrap_or_default(),
            before: value.get("before").and_then(Value::as_i64),
        })
    }
}

/// A saved page to read: the agent filter, the cursor, and the token that
/// fences a stale answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRead {
    pub agent: Option<String>,
    pub before: Option<i64>,
    pub generation: u64,
}

/// A short non-empty string (`text` in activity.ts).
pub(crate) fn text(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.chars().count() <= 256)
}

/// The envelope's children: a batch's events, or the envelope itself.
pub(crate) fn children(raw: &Value) -> Vec<&Value> {
    if raw.get("kind").and_then(Value::as_str) == Some("batch") {
        return raw
            .get("payload")
            .and_then(|payload| payload.get("events"))
            .and_then(Value::as_array)
            .map(|events| events.iter().collect())
            .unwrap_or_default();
    }
    vec![raw]
}

/// No envelope or batch child names a channel `allowed` refuses.
fn channels_allowed(raw: &Value, allowed: &dyn Fn(&str) -> bool) -> bool {
    let mut values = vec![raw];
    if raw.get("kind").and_then(Value::as_str) == Some("batch") {
        values.extend(children(raw));
    }
    !values
        .iter()
        .any(|value| text(value.get("channelId")).is_some_and(|channel| !allowed(channel)))
}

/// The live evidence and one saved page (`createAgentActivity`).
pub struct Activity {
    available: bool,
    leases: usize,
    status: FeedStatus,
    records: Vec<Arc<Record>>,
    bytes: usize,
    trimmed: usize,
    turns: Turns,
    /// Agents with live records: only they count as typing.
    live_agents: HashSet<String>,
    history: HistoryStatus,
    history_available: bool,
    history_generation: u64,
    history_loaded: bool,
    history_agent: Option<String>,
    history_before: Option<i64>,
    has_older: bool,
    history_older: bool,
    history_agents: Arc<[String]>,
    history_page: Option<Page>,
    capture: Capture,
    local: Arc<[LocalWork]>,
    snapshot: Snapshot,
}

impl Activity {
    /// `available` is whether this host can decode activity; `history`
    /// whether it has an archive to page through.
    pub fn new(available: bool, history: bool) -> Self {
        let status = if available {
            FeedStatus::Disabled
        } else {
            FeedStatus::Unavailable
        };
        let history_status = if history {
            HistoryStatus::Loading
        } else {
            HistoryStatus::Unavailable
        };
        Self {
            available,
            leases: 0,
            status,
            records: Vec::new(),
            bytes: 0,
            trimmed: 0,
            turns: Turns::default(),
            live_agents: HashSet::new(),
            history: history_status,
            history_available: history,
            history_generation: 0,
            history_loaded: false,
            history_agent: None,
            history_before: None,
            has_older: false,
            history_older: false,
            history_agents: Arc::new([]),
            history_page: None,
            capture: Capture::Unknown,
            local: Arc::new([]),
            snapshot: Snapshot {
                status,
                history: history_status,
                ..Default::default()
            },
        }
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Takes a lease (`activate`). The first starts the feed: the caller
    /// subscribes and reads the history this returns.
    pub fn activate(&mut self, now: u64) -> Option<HistoryRead> {
        if !self.available {
            return None;
        }
        self.leases += 1;
        if self.leases == 1 {
            return self.restart(now);
        }
        None
    }

    /// Drops a lease. The last one stops the feed and clears the display;
    /// true when the caller should unsubscribe.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn release(&mut self, now: u64) -> bool {
        if self.leases == 0 {
            return false;
        }
        self.leases -= 1;
        if self.leases == 0 {
            self.restart(now);
            return true;
        }
        false
    }

    fn reset(&mut self) {
        self.records.clear();
        self.bytes = 0;
        self.trimmed = 0;
        self.turns.reset();
        self.live_agents.clear();
    }

    /// Starts over: live evidence, the saved page and its cursor go
    /// (`restart`, also `accessChanged` and `clear`).
    pub fn restart(&mut self, now: u64) -> Option<HistoryRead> {
        self.history_generation += 1;
        self.history_loaded = false;
        self.history_before = None;
        self.history_page = None;
        self.has_older = false;
        self.history_older = false;
        self.history_agents = Arc::new([]);
        self.reset();
        let read = if self.available && self.leases > 0 {
            self.status = FeedStatus::Connecting;
            self.restore()
        } else {
            self.status = if self.available {
                FeedStatus::Disabled
            } else {
                FeedStatus::Unavailable
            };
            None
        };
        self.publish(now);
        read
    }

    /// The feed's state from the session (`state`).
    pub fn set_status(&mut self, next: FeedStatus, now: u64) {
        if !self.available || self.leases == 0 {
            return;
        }
        if self.status == FeedStatus::Listening && next != FeedStatus::Listening {
            self.turns.new_epoch();
        }
        if next != FeedStatus::Listening {
            self.turns.clear_typing();
        }
        self.status = next;
        self.publish(now);
    }

    pub fn set_capture(&mut self, capture: Capture, now: u64) {
        self.capture = capture;
        self.publish(now);
    }

    /// A decrypted live frame (`receive`). Frames naming a channel the
    /// viewer cannot read are dropped whole, batch children included.
    pub fn receive(&mut self, frame: Frame, now: u64, can_access: &dyn Fn(&str) -> bool) {
        if !self.available || self.leases == 0 {
            return;
        }
        let Ok(raw) = serde_json::from_str::<Value>(&frame.plaintext) else {
            return;
        };
        // Management requests are for owner review, never this display.
        if raw.get("kind").and_then(Value::as_str) == Some("agent_management_request") {
            return;
        }
        if !channels_allowed(&raw, can_access) {
            return;
        }
        self.admit(frame, &raw, now, false, now, can_access);
        self.publish(now);
    }

    fn admit(
        &mut self,
        frame: Frame,
        raw: &Value,
        received_at: u64,
        historical: bool,
        now: u64,
        can_access: &dyn Fn(&str) -> bool,
    ) {
        if !historical {
            self.live_agents.insert(frame.agent.clone());
            for item in children(raw) {
                self.trimmed += self
                    .turns
                    .fold(&frame.agent, item, now, can_access, &|_| None);
            }
        }
        if let Some(index) = self.records.iter().position(|r| r.id == frame.id) {
            if historical || !self.records[index].historical {
                return;
            }
            // A live copy replaces the saved one.
            self.records.remove(index);
        }
        let mut channel_ids: Vec<String> = Vec::new();
        let mut values = vec![raw];
        if raw.get("kind").and_then(Value::as_str) == Some("batch") {
            values.extend(children(raw));
        }
        for value in values {
            if let Some(channel) = text(value.get("channelId"))
                && !channel_ids.iter().any(|c| c == channel)
            {
                channel_ids.push(channel.to_string());
            }
        }
        let kind = text(raw.get("kind")).unwrap_or("unknown").to_string();
        let size = frame.plaintext.len();
        let record = Arc::new(Record {
            id: frame.id,
            agent: frame.agent,
            created_at: frame.created_at,
            plaintext: frame.plaintext.into(),
            received_at,
            historical,
            kind,
            channel_ids,
        });
        let at = self
            .records
            .partition_point(|r| r.received_at <= record.received_at);
        self.records.insert(at, record);
        if !historical {
            self.bytes += size;
        }
        while self.records.iter().filter(|r| !r.historical).count() > RECORD_LIMIT
            || self.bytes > BYTE_LIMIT
        {
            let Some(index) = self.records.iter().position(|r| !r.historical) else {
                break;
            };
            let first = self.records.remove(index);
            self.bytes -= first.plaintext.len();
            self.trimmed += 1;
        }
    }

    /// Who is typing now, from the session's typing state: `(agent,
    /// channel, thread root)`. Only agents with live records count, and
    /// only while the feed listens (`channelEvents`).
    pub fn typing(
        &mut self,
        entries: &[(String, String, Option<String>)],
        now: u64,
        can_access: &dyn Fn(&str) -> bool,
    ) {
        if self.leases == 0 || self.status != FeedStatus::Listening {
            return;
        }
        let entries: Vec<_> = entries
            .iter()
            .filter(|(agent, channel, _)| self.live_agents.contains(agent) && can_access(channel))
            .cloned()
            .collect();
        self.turns.typing(&entries, now);
        self.publish(now);
    }

    fn restore(&mut self) -> Option<HistoryRead> {
        if !self.history_available || self.leases == 0 || self.history_loaded {
            return None;
        }
        self.history_loaded = true;
        self.history = HistoryStatus::Loading;
        Some(HistoryRead {
            agent: self.history_agent.clone(),
            before: self.history_before,
            generation: self.history_generation,
        })
    }

    /// The answer to a [`HistoryRead`]; stale answers are ignored.
    pub fn history_loaded(
        &mut self,
        generation: u64,
        page: Result<Page, String>,
        now: u64,
        can_restore: &dyn Fn(&str) -> bool,
    ) {
        if generation != self.history_generation || self.leases == 0 {
            return;
        }
        match page {
            Ok(page) => {
                self.history_older = self.history_before.is_some();
                self.history_before = page.before;
                self.has_older = page.before.is_some();
                self.history_agents = page.agents.clone().into();
                self.history_page = Some(page);
                self.history = HistoryStatus::Ready;
                self.show_history_page(now, can_restore);
            }
            Err(_) => self.history = HistoryStatus::Error,
        }
        self.publish(now);
    }

    /// Replaces the saved display window with the current page; unsaved
    /// live records stay.
    fn show_history_page(&mut self, now: u64, can_restore: &dyn Fn(&str) -> bool) {
        self.records.retain(|record| !record.historical);
        let Some(page) = self.history_page.clone() else {
            return;
        };
        for (frame, received_at) in page.records.into_iter().rev() {
            let Ok(raw) = serde_json::from_str::<Value>(&frame.plaintext) else {
                continue;
            };
            if raw.get("kind").and_then(Value::as_str) == Some("agent_management_request")
                || !channels_allowed(&raw, can_restore)
            {
                continue;
            }
            self.admit(frame, &raw, received_at, true, now, can_restore);
        }
    }

    /// Re-checks the saved page after access became known.
    pub fn restore_history(&mut self, now: u64, can_restore: &dyn Fn(&str) -> bool) {
        if self.history_page.is_some() && self.leases > 0 {
            self.show_history_page(now, can_restore);
            self.publish(now);
        }
    }

    /// Pages this agent's saved activity (`selectHistory`); "" is every
    /// agent.
    pub fn select_history(&mut self, agent: &str, now: u64) -> Option<HistoryRead> {
        let agent = (!agent.is_empty()).then(|| agent.to_string());
        if self.history_agent == agent {
            return None;
        }
        self.history_agent = agent;
        self.history_before = None;
        self.history_page = None;
        self.has_older = false;
        self.history_older = false;
        self.records.retain(|record| !record.historical);
        self.history_generation += 1;
        self.history_loaded = false;
        let read = self.restore();
        self.publish(now);
        read
    }

    /// "Show latest saved activity".
    pub fn latest_history(&mut self, now: u64) -> Option<HistoryRead> {
        if self.history == HistoryStatus::Loading {
            return None;
        }
        self.history_before = None;
        self.history_loaded = false;
        let read = self.restore();
        self.publish(now);
        read
    }

    /// "Load older activity".
    pub fn load_older(&mut self, now: u64) -> Option<HistoryRead> {
        if self.history == HistoryStatus::Loading || !self.has_older {
            return None;
        }
        self.history_loaded = false;
        let read = self.restore();
        self.publish(now);
        read
    }

    /// This scope's local work, already filtered to readable channels.
    pub fn set_local(&mut self, local: Arc<[LocalWork]>, now: u64) -> bool {
        if *local == *self.local {
            return false;
        }
        self.local = local;
        self.publish(now)
    }

    /// Trigger ids still waiting for their message.
    pub fn unresolved(&self) -> Vec<String> {
        self.turns.unresolved()
    }

    /// Messages the session found for unresolved triggers.
    pub fn messages_found(&mut self, found: &[TriggerMessage], now: u64) -> bool {
        let find = |id: &str| found.iter().find(|event| event.id == id).cloned();
        self.turns.resolve(&find) && self.publish(now)
    }

    /// Recomputes the snapshot: drops records past the age limit and
    /// expired typing, and re-evaluates turn states (`publish`). True
    /// when anything visible changed.
    pub fn publish(&mut self, now: u64) -> bool {
        let before = self.records.len();
        self.records
            .retain(|record| now.saturating_sub(record.received_at) < HISTORY_AGE_MS);
        if self.records.len() != before {
            self.trimmed += before - self.records.len();
            self.bytes = self
                .records
                .iter()
                .filter(|r| !r.historical)
                .map(|r| r.plaintext.len())
                .sum();
        }
        let retained: HashSet<&str> = self
            .records
            .iter()
            .filter(|r| !r.historical)
            .map(|r| r.agent.as_str())
            .collect();
        self.live_agents
            .retain(|agent| retained.contains(agent.as_str()));
        self.turns.expire_typing(now, &self.live_agents);
        let listening = self.status == FeedStatus::Listening;
        let turns = self.turns.states(now, listening);
        let typing = self.turns.working_typing();
        let mut working = turns::working_channels(&turns, &typing);
        working.extend(
            self.local
                .iter()
                .filter(|row| row.state.active())
                .map(|row| row.channel_id.clone()),
        );
        working.sort();
        working.dedup();
        let (records, requested): (Arc<[Arc<Record>]>, _) = if self.snapshot.records.len()
            == self.records.len()
            && self
                .snapshot
                .records
                .iter()
                .zip(&self.records)
                .all(|(a, b)| Arc::ptr_eq(a, b))
        {
            (
                self.snapshot.records.clone(),
                self.snapshot.requested.clone(),
            )
        } else {
            (self.records.clone().into(), requested(&self.records).into())
        };
        let next = Snapshot {
            status: self.status,
            records,
            turns: turns.into(),
            local: self.local.clone(),
            typing: typing.into(),
            trimmed: self.trimmed,
            history: self.history,
            has_older: self.has_older,
            history_older: self.history_older,
            history_skipped: self.history_page.as_ref().map_or(0, |page| page.skipped),
            history_agents: self.history_agents.clone(),
            capture: self.capture,
            working_channels: working.into(),
            requested,
        };
        if next == self.snapshot {
            return false;
        }
        self.snapshot = next;
        true
    }
}

/// The `(channel, message)` pairs live turn starts name as triggers.
fn requested(records: &[Arc<Record>]) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for record in records.iter().filter(|r| !r.historical) {
        if record.kind != "turn_started" && record.kind != "batch" {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<Value>(&record.plaintext) else {
            continue;
        };
        let mut values = vec![&raw];
        if record.kind == "batch" {
            values.extend(children(&raw));
        }
        for value in values {
            if value.get("kind").and_then(Value::as_str) != Some("turn_started") {
                continue;
            }
            let Some(channel) = text(value.get("channelId")) else {
                continue;
            };
            let ids = value
                .get("payload")
                .and_then(|p| p.get("triggeringEventIds"))
                .and_then(Value::as_array);
            for id in ids.into_iter().flatten().filter_map(Value::as_str) {
                let pair = (channel.to_string(), id.to_string());
                if hex64(id) && !pairs.contains(&pair) {
                    pairs.push(pair);
                }
            }
        }
    }
    pairs
}

/// An open activity panel's subject (`ActivitySelection`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub agent: String,
    pub channel: Option<String>,
    /// Exact thread root within `channel`.
    pub thread: Option<String>,
}

/// `buzz:agent-activity?agent=…&channel=…&thread=…` (`activityTarget`):
/// an internal panel target, not an OS link or an access grant.
pub fn target(agent: &str, channel: Option<&str>, thread: Option<&str>) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("agent", agent);
    if let Some(channel) = channel.filter(|c| !c.is_empty()) {
        query.append_pair("channel", channel);
        if let Some(thread) = thread {
            query.append_pair("thread", thread);
        }
    }
    format!("buzz:agent-activity?{}", query.finish())
}

/// Parses a [`target`], refusing anything ambiguous (`activitySelection`).
pub fn selection(target: &str) -> Option<Selection> {
    let url = url::Url::parse(target).ok()?;
    if url.scheme() != "buzz"
        || url.path() != "agent-activity"
        || url.host().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let mut agent = Vec::new();
    let mut channel = Vec::new();
    let mut thread = Vec::new();
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "agent" => agent.push(value.into_owned()),
            "channel" => channel.push(value.into_owned()),
            "thread" => thread.push(value.into_owned()),
            _ => return None,
        }
    }
    if agent.len() != 1 || channel.len() > 1 || thread.len() > 1 || !hex64(&agent[0]) {
        return None;
    }
    let channel = channel.pop();
    let thread = thread.pop();
    if channel
        .as_ref()
        .is_some_and(|c| c.is_empty() || c.len() > 256)
        || thread
            .as_ref()
            .is_some_and(|t| channel.is_none() || !hex64(t))
    {
        return None;
    }
    Some(Selection {
        agent: agent.pop()?,
        channel,
        thread,
    })
}

/// A Raw view entry (`ActivityRecord`): a record, or one batch child of
/// it in a channel scope.
#[derive(Clone, Debug, PartialEq)]
pub struct RawRecord {
    pub id: String,
    pub envelope_id: String,
    pub agent: String,
    pub received_at: u64,
    pub kind: String,
    pub plaintext: Arc<str>,
}

/// The Raw view's entries for `agent` (`activityRecords`). In a channel
/// scope a batch is split into its matching children, never relabelled.
pub fn raw_records(records: &[Arc<Record>], agent: &str, channel: Option<&str>) -> Vec<RawRecord> {
    let mut out = Vec::new();
    for record in records.iter().filter(|r| r.agent == agent) {
        let Some(channel) = channel.filter(|c| !c.is_empty()) else {
            out.push(RawRecord {
                id: record.id.clone(),
                envelope_id: record.id.clone(),
                agent: record.agent.clone(),
                received_at: record.received_at,
                kind: record.kind.clone(),
                plaintext: record.plaintext.clone(),
            });
            continue;
        };
        let Ok(raw) = serde_json::from_str::<Value>(&record.plaintext) else {
            continue;
        };
        if raw.get("kind").and_then(Value::as_str) != Some("batch") {
            if raw.get("channelId").and_then(Value::as_str) == Some(channel) {
                out.push(RawRecord {
                    id: record.id.clone(),
                    envelope_id: record.id.clone(),
                    agent: record.agent.clone(),
                    received_at: record.received_at,
                    kind: record.kind.clone(),
                    plaintext: record.plaintext.clone(),
                });
            }
            continue;
        }
        let Some(events) = raw
            .get("payload")
            .and_then(|p| p.get("events"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for (index, child) in events.iter().enumerate() {
            if child.get("channelId").and_then(Value::as_str) != Some(channel) {
                continue;
            }
            out.push(RawRecord {
                id: format!("{}:{index}", record.id),
                envelope_id: record.id.clone(),
                agent: record.agent.clone(),
                received_at: record.received_at,
                kind: child
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                plaintext: serde_json::to_string_pretty(child)
                    .unwrap_or_default()
                    .into(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests;
