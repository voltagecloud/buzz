//! Mission control's picture of the fleet, free of GPUI so it can be
//! tested: what each agent does now, from its runner's state and its
//! observed activity; whether it needs its owner; the order the page
//! lists agents in; and which changes deserve a notification.

use crate::agent_activity::current::{tool_label, working_duration};
use crate::agent_activity::transcript_model::{ItemKind, Scope, TranscriptTurn, transcript};
use crate::agent_activity::turns::TurnState;
use crate::agent_activity::{FeedStatus, Snapshot};
use buzz_relay_client::archive::ArchiveRecord;
use buzz_relay_client::usage::decode_usage;
use std::collections::{HashMap, HashSet};

/// A turn with no reply, thought or tool update for this long is stuck...
pub const STUCK_MS: u64 = 5 * 60_000;
/// ...or this long while a tool runs, since builds and tests are quiet.
pub const TOOL_STUCK_MS: u64 = 15 * 60_000;
/// Connecting to its community for this long is stuck.
pub const TRANSITION_STUCK_MS: u64 = 2 * 60_000;
/// A failed last turn or run needs the owner for this long, or until
/// another one starts.
pub const TURN_ERROR_MS: u64 = 30 * 60_000;
/// Open turns older than this are lost evidence, not current work.
pub const ABANDONED_MS: u64 = 6 * 60 * 60_000;
/// Usage covers this much time.
pub const USAGE_WINDOW_MS: u64 = 24 * 60 * 60_000;

/// A host connection as Mission control shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Link {
    Connected,
    Connecting,
    Offline,
    /// The host refused this app: a bad token or an incompatible version.
    Refused,
}

impl Link {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connected => "Connected",
            Self::Connecting => "Connecting",
            Self::Offline => "Offline",
            Self::Refused => "Refused",
        }
    }
}

/// What an agent's runner is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    /// A run is in progress in this app.
    Working,
    /// This app runs it and waits for something to answer.
    Ready,
    /// Another app on its host runs it.
    Elsewhere,
    /// Its community is not connected yet, so it hears nothing.
    Connecting,
}

/// An agent's runner as Mission control reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub status: Status,
    /// When it entered its status, Unix ms: the run's start while working.
    pub since: Option<u64>,
    /// The last run's failure: when it ended, Unix ms, and why.
    pub failed: Option<(u64, String)>,
}

/// An agent's open turn, from observed activity.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenTurn {
    pub started_at: u64,
    /// The newest reply, thought or tool update, else the start.
    pub progress_at: u64,
    /// The newest evidence of any kind, heartbeats included.
    pub evidence_at: u64,
    /// What it is doing, as the composer's working popover says it.
    pub label: String,
    pub waiting_permission: bool,
    pub tool_running: bool,
}

/// What observed activity says about one agent, across communities.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    pub turn: Option<OpenTurn>,
    /// The newest evidence of any turn, ms.
    pub last_seen: Option<u64>,
    /// Where its newest turn ran: the community origin and channel.
    pub place: Option<(String, String)>,
    /// The newest turn's error, with when it ended.
    pub failed_turn: Option<(u64, String)>,
    /// Turns started within the usage window that the live window holds.
    pub recent_turns: usize,
    /// The feed listened, so silence means something.
    pub listening: bool,
}

/// The label for a turn's newest state (`currentActivity`'s tool labels,
/// plus what a turn without a running tool is doing).
fn turn_label(turn: &TranscriptTurn) -> (String, bool, bool) {
    let tools: Vec<_> = turn
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Tool(tool) if matches!(tool.status.as_str(), "in_progress" | "pending") => {
                Some(tool)
            }
            _ => None,
        })
        .collect();
    let waiting = turn.items.iter().any(|item| {
        matches!(&item.kind, ItemKind::Tool(tool)
            if tool.permission.as_ref().is_some_and(|p| p.outcome.is_none()))
    });
    let label = if waiting {
        "Waiting for permission".to_string()
    } else if tools.len() > 1 {
        format!("Running {} tools", tools.len())
    } else if let Some(tool) = tools.first() {
        tool_label(tool)
    } else {
        match turn.items.last().map(|item| &item.kind) {
            Some(ItemKind::Message { .. }) => "Writing a reply",
            Some(ItemKind::Thought { .. }) => "Thinking",
            Some(ItemKind::Plan(_)) => "Planning",
            _ => "Working",
        }
        .to_string()
    };
    (label, waiting, !tools.is_empty())
}

/// What one community's transcripts say about an agent, whatever the
/// time: [`Evidence::at`] applies the time windows, so a cached one still
/// ages.
#[derive(Clone, Debug)]
pub struct Evidence {
    /// The newest turn not seen to end.
    open: Option<OpenTurn>,
    starts: Vec<u64>,
    last_seen: u64,
    place: Option<(String, String)>,
    failed_turn: Option<(u64, String)>,
    listening: bool,
}

impl Evidence {
    /// The evidence as of `now`.
    pub fn at(&self, now: u64) -> Live {
        Live {
            // Older open turns are older still, so none of them counts.
            turn: self
                .open
                .clone()
                .filter(|turn| now.saturating_sub(turn.evidence_at) < ABANDONED_MS),
            last_seen: Some(self.last_seen),
            place: self.place.clone(),
            failed_turn: self.failed_turn.clone(),
            recent_turns: self
                .starts
                .iter()
                .filter(|start| now.saturating_sub(**start) < USAGE_WINDOW_MS)
                .count(),
            listening: self.listening,
        }
    }
}

/// One community's evidence about `agent`, or `None` without any.
pub fn evidence_in(snapshot: &Snapshot, origin: &str, agent: &str) -> Option<Evidence> {
    let turns: Vec<_> = snapshot.turns.iter().filter(|t| t.agent == agent).collect();
    if turns.is_empty() {
        return None;
    }
    let transcript = transcript(
        &snapshot.records,
        &Scope {
            agent: agent.to_string(),
            channel: None,
            thread: None,
        },
    );
    let newest = turns.iter().max_by_key(|t| t.timestamp)?;
    let open = turns
        .iter()
        .filter(|t| {
            t.state != TurnState::Ended
                && !transcript
                    .turns
                    .iter()
                    .any(|turn| turn.turn_id == t.turn_id && turn.ended_at.is_some())
        })
        .max_by_key(|t| t.timestamp)
        .map(|evidence| {
            let turn = transcript
                .turns
                .iter()
                .find(|turn| turn.turn_id == evidence.turn_id);
            let started_at = turn.map_or(evidence.timestamp, |turn| turn.started_at);
            let progress_at = turn
                .and_then(|turn| turn.items.iter().map(|item| item.at).max())
                .unwrap_or(started_at)
                .max(started_at);
            let (label, waiting_permission, tool_running) =
                turn.map_or(("Working".into(), false, false), turn_label);
            OpenTurn {
                started_at,
                progress_at,
                evidence_at: evidence.timestamp,
                label,
                waiting_permission,
                tool_running,
            }
        });
    let failed_turn = transcript
        .turns
        .iter()
        .max_by_key(|turn| turn.started_at)
        .and_then(|turn| Some((turn.ended_at?, turn.error.clone()?)));
    Some(Evidence {
        open,
        starts: transcript
            .turns
            .iter()
            .map(|turn| turn.started_at)
            .collect(),
        last_seen: newest.timestamp,
        place: newest
            .channel_id
            .clone()
            .map(|channel| (origin.to_string(), channel)),
        failed_turn,
        listening: snapshot.status == FeedStatus::Listening,
    })
}

/// Joins what several communities saw: the newest of each.
pub fn merge(lives: impl IntoIterator<Item = Live>) -> Option<Live> {
    let mut merged: Option<Live> = None;
    for live in lives {
        let Some(into) = merged.as_mut() else {
            merged = Some(live);
            continue;
        };
        let newer = live.last_seen > into.last_seen;
        if live.turn.as_ref().map(|t| t.evidence_at) > into.turn.as_ref().map(|t| t.evidence_at) {
            into.turn = live.turn;
        }
        if live.failed_turn.as_ref().map(|f| f.0) > into.failed_turn.as_ref().map(|f| f.0) {
            into.failed_turn = live.failed_turn;
        }
        if newer {
            into.last_seen = live.last_seen;
            into.place = live.place;
        }
        into.recent_turns += live.recent_turns;
        into.listening |= live.listening;
    }
    merged
}

/// Why an agent needs its owner, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Need {
    Failed,
    Waiting,
    Stuck,
}

impl Need {
    pub fn label(self) -> &'static str {
        match self {
            Self::Failed => "Failed",
            Self::Waiting => "Needs you",
            Self::Stuck => "Stuck",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attention {
    pub need: Need,
    pub reason: String,
}

fn attention_of(need: Need, reason: impl Into<String>) -> Option<Attention> {
    Some(Attention {
        need,
        reason: reason.into(),
    })
}

/// The first line of an error, at most 160 characters.
pub fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let line = line.trim();
    if line.chars().count() > 160 {
        format!("{}…", line.chars().take(159).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Whether the agent needs its owner now. Agents of a host that is not
/// connected show their last known state, which proves nothing.
pub fn attention(run: &Run, live: Option<&Live>, link: Link, now: u64) -> Option<Attention> {
    if link != Link::Connected {
        return None;
    }
    if run.status == Status::Connecting {
        let since = run.since?;
        return (now.saturating_sub(since) >= TRANSITION_STUCK_MS).then(|| Attention {
            need: Need::Stuck,
            reason: format!("Connecting for {}", working_duration(since, now)),
        });
    }
    if let Some(turn) = live.and_then(|live| live.turn.as_ref()) {
        if turn.waiting_permission {
            return attention_of(Need::Waiting, "Waiting for permission");
        }
        let limit = if turn.tool_running {
            TOOL_STUCK_MS
        } else {
            STUCK_MS
        };
        let listening = live.is_some_and(|live| live.listening);
        if listening && now.saturating_sub(turn.progress_at) >= limit {
            return attention_of(
                Need::Stuck,
                format!(
                    "No progress for {}",
                    working_duration(turn.progress_at, now)
                ),
            );
        }
        return None;
    }
    if run.status == Status::Working {
        let since = run.since?;
        return (now.saturating_sub(since) >= TOOL_STUCK_MS).then(|| Attention {
            need: Need::Stuck,
            reason: format!("No progress for {}", working_duration(since, now)),
        });
    }
    // The newer of the last failed run and the last failed turn.
    let turn = live.and_then(|live| live.failed_turn.as_ref());
    let (ended, reason) = match (&run.failed, turn) {
        (Some(run), Some(turn)) if turn.0 > run.0 => (turn.0, turn_failed(&turn.1)),
        (Some(run), _) => (run.0, format!("Run failed: {}", first_line(&run.1))),
        (None, Some(turn)) => (turn.0, turn_failed(&turn.1)),
        (None, None) => return None,
    };
    (now.saturating_sub(ended) < TURN_ERROR_MS)
        .then(|| attention_of(Need::Failed, reason))
        .flatten()
}

fn turn_failed(error: &str) -> String {
    format!("Turn failed: {}", first_line(error))
}

/// "just now", "5m ago", "3h ago", "2d ago".
pub fn ago(at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(at) / 1000;
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// What the row says the agent is doing, and how long it has.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doing {
    pub text: String,
    pub elapsed: Option<String>,
    /// A turn is open.
    pub active: bool,
}

pub fn doing(run: &Run, live: Option<&Live>, link: Link, now: u64) -> Doing {
    let last = live
        .and_then(|live| live.last_seen)
        .map(|at| format!("last active {}", ago(at, now)));
    let elapsed = |at: Option<u64>| at.map(|at| working_duration(at, now));
    if link != Link::Connected {
        return Doing {
            text: format!("Last known: {}", status_label(run.status).to_lowercase()),
            ..Default::default()
        };
    }
    let idle = |text: &str| Doing {
        text: match &last {
            Some(last) => format!("{text} · {last}"),
            None => text.into(),
        },
        ..Default::default()
    };
    match (run.status, live.and_then(|live| live.turn.as_ref())) {
        (Status::Connecting, _) => Doing {
            text: "Connecting to its community…".into(),
            elapsed: elapsed(run.since),
            active: false,
        },
        (_, Some(turn)) => Doing {
            text: turn.label.clone(),
            elapsed: elapsed(Some(turn.started_at)),
            active: true,
        },
        (Status::Working, None) => Doing {
            text: "Working".into(),
            elapsed: elapsed(run.since),
            active: true,
        },
        (Status::Ready, None) => idle("Idle"),
        (Status::Elsewhere, None) => idle("Runs in another app"),
    }
}

pub fn status_label(status: Status) -> &'static str {
    match status {
        Status::Working => "Working",
        Status::Ready => "Ready",
        Status::Elsewhere => "Elsewhere",
        Status::Connecting => "Connecting",
    }
}

/// Recent usage from saved turn metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub turns: u64,
    pub tokens: Option<u64>,
    pub cost: Option<f64>,
}

/// Each agent's turns, tokens and cost reported since `since_ms`, from
/// its saved metrics in every channel. A turn reported twice counts once.
pub fn usage_by_agent(records: &[ArchiveRecord], since_ms: i64) -> HashMap<String, Usage> {
    let mut seen = HashSet::new();
    let mut usage: HashMap<String, Usage> = HashMap::new();
    for record in records {
        let Some(channel) = serde_json::from_str::<serde_json::Value>(&record.plaintext)
            .ok()
            .and_then(|raw| Some(raw.get("channelId")?.as_str()?.to_string()))
        else {
            continue;
        };
        let Some(item) = decode_usage(record, &channel) else {
            continue;
        };
        if item.timestamp < since_ms {
            continue;
        }
        let key = match (&item.session_id, &item.turn_id) {
            (Some(session), Some(turn)) => format!("{}:{session}:{turn}", item.agent),
            _ => item.id.clone(),
        };
        if !seen.insert(key) {
            continue;
        }
        let entry = usage.entry(item.agent.clone()).or_default();
        entry.turns += 1;
        let Some(turn) = item.turn else {
            continue;
        };
        let tokens = turn
            .total_tokens
            .or(match (turn.input_tokens, turn.output_tokens) {
                (None, None) => None,
                (input, output) => Some(input.unwrap_or(0) + output.unwrap_or(0)),
            });
        if let Some(tokens) = tokens {
            *entry.tokens.get_or_insert(0) += tokens;
        }
        if let Some(cost) = turn.cost_usd {
            *entry.cost.get_or_insert(0.) += cost;
        }
    }
    usage
}

/// `950`, `12.3k`, `4.5M`.
pub fn compact(value: u64) -> String {
    match value {
        0..1000 => value.to_string(),
        1000..999_950 => trim(format!("{:.1}k", value as f64 / 1000.)),
        _ => trim(format!("{:.1}M", value as f64 / 1_000_000.)),
    }
}

fn trim(text: String) -> String {
    text.replace(".0k", "k").replace(".0M", "M")
}

/// "12 turns · 340k tokens · $0.42", from metrics when saved, else the
/// turns the live window holds.
pub fn usage_line(usage: Option<&Usage>, live: Option<&Live>) -> Option<String> {
    let plural = |n: u64| format!("{n} turn{}", if n == 1 { "" } else { "s" });
    match usage {
        Some(usage) if usage.turns > 0 => {
            let mut parts = vec![plural(usage.turns)];
            if let Some(tokens) = usage.tokens {
                parts.push(format!("{} tokens", compact(tokens)));
            }
            if let Some(cost) = usage.cost.filter(|cost| *cost > 0.) {
                parts.push(format!("${cost:.2}"));
            }
            Some(parts.join(" · "))
        }
        _ => live
            .map(|live| live.recent_turns as u64)
            .filter(|turns| *turns > 0)
            .map(plural),
    }
}

/// An agent on a host.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub host: u64,
    pub id: String,
}

/// One row of the page.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: Key,
    pub host_label: String,
    pub remote: bool,
    pub pubkey: String,
    pub name: String,
    pub picture: Option<String>,
    /// Its type's name.
    pub model: Option<String>,
    pub status: Status,
    pub link: Link,
    pub attention: Option<Attention>,
    pub doing: Doing,
    pub usage: Option<String>,
    /// Where its newest turn ran: the community origin and channel.
    pub place: Option<(String, String)>,
}

/// Where an entry sorts: attention first, by urgency; then agents at work,
/// ready, run elsewhere, connecting; agents of unreachable hosts last.
fn group(entry: &Entry) -> u8 {
    if entry.link != Link::Connected {
        return 9;
    }
    if let Some(attention) = &entry.attention {
        return attention.need as u8;
    }
    match entry.status {
        _ if entry.doing.active => 3,
        Status::Working | Status::Ready => 4,
        Status::Elsewhere => 5,
        Status::Connecting => 6,
    }
}

pub fn sort(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        (group(a), a.name.to_lowercase(), &a.host_label, &a.key).cmp(&(
            group(b),
            b.name.to_lowercase(),
            &b.host_label,
            &b.key,
        ))
    });
}

/// One host in the summary.
#[derive(Clone, Debug, PartialEq)]
pub struct HostSummary {
    pub key: u64,
    pub label: String,
    pub address: Option<String>,
    pub link: Link,
    /// Why it is offline or refused.
    pub detail: Option<String>,
    pub total: usize,
    /// Agents at work now.
    pub working: usize,
    pub attention: usize,
}

/// Everything the page shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Board {
    pub hosts: Vec<HostSummary>,
    pub entries: Vec<Entry>,
}

impl Board {
    pub fn attention(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.attention.is_some())
            .count()
    }
}

/// A change into needing attention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub key: Key,
    pub name: String,
    pub host_label: String,
    pub attention: Attention,
}

/// Remembers what each agent needed, to tell when one starts needing its
/// owner. A host's first snapshot is the baseline, so opening the app
/// does not announce what was already wrong.
#[derive(Default)]
pub struct Tracker {
    needs: HashMap<Key, Need>,
    baselined: HashSet<u64>,
}

impl Tracker {
    /// Takes the newest entries and returns the agents that came to need
    /// attention since the last call. `quiet` records without alerting,
    /// while evidence is still arriving after startup.
    pub fn observe(&mut self, entries: &[Entry], quiet: bool) -> Vec<Alert> {
        let mut alerts = Vec::new();
        let mut fresh: HashSet<u64> = HashSet::new();
        let present: HashSet<&Key> = entries.iter().map(|entry| &entry.key).collect();
        self.needs.retain(|key, _| present.contains(key));
        for entry in entries {
            // An unreachable host's last state is no news either way.
            if entry.link != Link::Connected {
                continue;
            }
            let host = entry.key.host;
            let baseline = !self.baselined.contains(&host);
            if baseline {
                fresh.insert(host);
            }
            let need = entry.attention.as_ref().map(|a| a.need);
            let before = match need {
                Some(need) => self.needs.insert(entry.key.clone(), need),
                None => self.needs.remove(&entry.key),
            };
            if let Some(attention) = &entry.attention
                && before != need
                && !baseline
                && !quiet
            {
                alerts.push(Alert {
                    key: entry.key.clone(),
                    name: entry.name.clone(),
                    host_label: entry.host_label.clone(),
                    attention: attention.clone(),
                });
            }
        }
        self.baselined.extend(fresh);
        alerts
    }

    /// Forgets a host's baseline, as when it is removed.
    pub fn forget_host(&mut self, host: u64) {
        self.baselined.remove(&host);
        self.needs.retain(|key, _| key.host != host);
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
