//! Which agents are handling which message (`request-activity.ts`,
//! `request-work.ts`, `thread-working.ts`, `message-activity.ts`): only
//! explicit triggering ids tie work to a message. This app's own agents
//! report it locally; other agents' telemetry names the messages that
//! started each turn. Typing never says which message an agent handles.

use super::local::{LocalWork, LocalWorkState, WorkStep, breathing, local_activity};
use super::transcript_model::{ItemKind, TranscriptTurn};
use super::turns::{ActivityTurn, TurnState, parse_time};
use super::{FeedStatus, Record, Snapshot, raw_records};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// A request's state, from local work or from telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkState {
    Waiting,
    Working,
    Done,
    Error,
    Superseded,
    /// Telemetry cannot tell.
    Unknown,
    /// Telemetry saw every linked turn end.
    Ended,
}

impl From<LocalWorkState> for WorkState {
    fn from(state: LocalWorkState) -> Self {
        match state {
            LocalWorkState::Waiting => Self::Waiting,
            LocalWorkState::Working => Self::Working,
            LocalWorkState::Done => Self::Done,
            LocalWorkState::Error => Self::Error,
            LocalWorkState::Superseded => Self::Superseded,
        }
    }
}

/// The telemetry turns a request started (`requestActivity`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selected {
    pub turn_ids: BTreeSet<String>,
    /// The first admitted terminal frame of each turn: ended or not.
    pub terminal: HashMap<String, bool>,
    /// Starts were duplicated, reset or untimed: no outcome can be told.
    pub uncertain: bool,
}

/// Turns whose start names one of `ids`, from captured trigger ids only:
/// no inference from time or names, and no wider fallback when the start
/// is missing.
pub fn request_activity(
    records: &[Arc<Record>],
    agent: &str,
    channel: &str,
    ids: &[&str],
) -> Selected {
    let mut selected = Selected::default();
    let wanted: HashSet<&str> = ids.iter().copied().filter(|id| super::hex64(id)).collect();
    if wanted.is_empty() {
        return selected;
    }
    struct Parsed {
        kind: String,
        turn: String,
        seq: Option<i64>,
        admitted: bool,
        triggers: Vec<String>,
    }
    let parsed: Vec<Parsed> = raw_records(records, agent, Some(channel))
        .into_iter()
        .filter_map(|record| {
            let value: Value = serde_json::from_str(&record.plaintext).ok()?;
            let turn = value.get("turnId")?.as_str()?;
            if turn.is_empty() || turn.len() > 256 {
                return None;
            }
            let admitted = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_time)
                .is_some_and(|at| at <= record.received_at + 5000);
            Some(Parsed {
                kind: value
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                turn: turn.into(),
                seq: value
                    .get("seq")
                    .and_then(Value::as_i64)
                    .filter(|seq| *seq >= 0 && *seq < (1 << 53)),
                admitted,
                triggers: value
                    .get("payload")
                    .and_then(|p| p.get("triggeringEventIds"))
                    .and_then(Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| id.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect();
    for start in parsed.iter().filter(|p| p.kind == "turn_started") {
        if !start.triggers.iter().any(|id| wanted.contains(id.as_str())) {
            continue;
        }
        let turn: Vec<&Parsed> = parsed.iter().filter(|p| p.turn == start.turn).collect();
        // Duplicate starts or sequence resets leave the turn ambiguous.
        if turn.iter().filter(|p| p.kind == "turn_started").count() != 1
            || turn.iter().any(|p| p.seq.is_none())
        {
            selected.uncertain = true;
            continue;
        }
        let Some(from) = start.seq else {
            continue;
        };
        let after: Vec<i64> = turn
            .iter()
            .filter_map(|p| p.seq)
            .filter(|seq| *seq >= from)
            .collect();
        if after.iter().collect::<HashSet<_>>().len() != after.len() {
            selected.uncertain = true;
            continue;
        }
        selected.turn_ids.insert(start.turn.clone());
        if !start.admitted {
            selected.uncertain = true;
            continue;
        }
        // The first admitted terminal frame in receipt order, not the
        // largest seq: a later error must not rewrite an earlier end.
        let end = turn.iter().find(|p| {
            matches!(
                p.kind.as_str(),
                "turn_completed" | "turn_error" | "agent_panic"
            ) && p.admitted
        });
        if let Some(end) = end
            && end.seq.is_some_and(|seq| seq >= from)
        {
            selected
                .terminal
                .insert(start.turn.clone(), end.kind == "turn_completed");
        }
    }
    selected
}

/// Lifecycle evidence for the selected turns, never a claim that a reply
/// was delivered: working wins; every linked turn must agree before an
/// end; any trimming may have erased a linked start (`requestActivityState`).
pub fn request_activity_state(
    selected: &Selected,
    turns: &[ActivityTurn],
    trimmed: usize,
) -> WorkState {
    let linked: Vec<Option<&ActivityTurn>> = selected
        .turn_ids
        .iter()
        .map(|id| turns.iter().find(|turn| turn.turn_id == *id))
        .collect();
    if linked
        .iter()
        .any(|turn| turn.is_some_and(|t| t.state == TurnState::Working))
    {
        return WorkState::Working;
    }
    if trimmed > 0
        || selected.uncertain
        || linked
            .iter()
            .any(|turn| turn.is_none_or(|t| t.state == TurnState::Unknown))
    {
        return WorkState::Unknown;
    }
    if linked.is_empty() {
        return WorkState::Waiting;
    }
    if linked.iter().any(|turn| {
        turn.is_none_or(|t| {
            t.state != TurnState::Ended || !selected.terminal.contains_key(&t.turn_id)
        })
    }) {
        return WorkState::Unknown;
    }
    if selected.terminal.values().any(|ended| !ended) {
        WorkState::Error
    } else {
        WorkState::Ended
    }
}

/// One agent's handling of one or more messages (`RequestWork`).
#[derive(Clone, Debug, PartialEq)]
pub struct RequestWork {
    pub agent: String,
    pub state: WorkState,
    pub reason: Option<String>,
    /// Latest live local step.
    pub step: Option<WorkStep>,
    /// Reported by this app's runtime rather than telemetry.
    pub lifecycle: bool,
    /// A failure the owner hid on this device.
    pub dismissed: bool,
    pub selected: Selected,
    pub turns: Vec<ActivityTurn>,
    pub working: bool,
}

/// Each agent handling any of `ids` in `channel`, sorted by key.
pub fn request_work(snapshot: &Snapshot, channel: &str, ids: &[&str]) -> Vec<RequestWork> {
    let live: Vec<Arc<Record>> = snapshot
        .records
        .iter()
        .filter(|record| !record.historical)
        .cloned()
        .collect();
    let requests: Vec<&LocalWork> = snapshot
        .local
        .iter()
        .filter(|row| row.channel_id == channel && ids.contains(&row.message_id.as_str()))
        .collect();
    let agents: BTreeSet<&str> = live
        .iter()
        .filter(|record| record.channel_ids.iter().any(|c| c == channel))
        .map(|record| record.agent.as_str())
        .chain(requests.iter().map(|row| row.agent.as_str()))
        .collect();
    agents
        .into_iter()
        .filter_map(|agent| {
            let local: Vec<&&LocalWork> =
                requests.iter().filter(|row| row.agent == agent).collect();
            if !local.is_empty() {
                let chosen = [
                    LocalWorkState::Working,
                    LocalWorkState::Waiting,
                    LocalWorkState::Error,
                ]
                .iter()
                .find_map(|state| local.iter().find(|row| row.state == *state))
                .unwrap_or(&local[0]);
                return Some(RequestWork {
                    agent: agent.to_string(),
                    state: chosen.state.into(),
                    reason: chosen.error.clone(),
                    step: chosen.step.clone(),
                    lifecycle: true,
                    dismissed: chosen.dismissed,
                    selected: Selected::default(),
                    turns: Vec::new(),
                    working: chosen.state == LocalWorkState::Working,
                });
            }
            let selected = request_activity(&live, agent, channel, ids);
            if selected.turn_ids.is_empty() {
                return None;
            }
            let turns: Vec<ActivityTurn> = snapshot
                .turns
                .iter()
                .filter(|turn| {
                    turn.agent == agent
                        && turn.channel_id.as_deref() == Some(channel)
                        && selected.turn_ids.contains(&turn.turn_id)
                })
                .cloned()
                .collect();
            let state = request_activity_state(&selected, &turns, snapshot.trimmed);
            Some(RequestWork {
                agent: agent.to_string(),
                state,
                reason: None,
                step: None,
                lifecycle: false,
                dismissed: false,
                selected,
                turns,
                working: snapshot.status == FeedStatus::Listening && state == WorkState::Working,
            })
        })
        .collect()
}

/// Messages in `channel` that may have work tied to them: local requests
/// and triggers telemetry named.
pub fn candidates(snapshot: &Snapshot, channel: &str) -> BTreeSet<String> {
    snapshot
        .local
        .iter()
        .filter(|row| row.channel_id == channel)
        .map(|row| row.message_id.clone())
        .chain(
            snapshot
                .requested
                .iter()
                .filter(|(c, _)| c == channel)
                .map(|(_, id)| id.clone()),
        )
        .collect()
}

/// Thread roots in `channel` that may have work in them: local requests'
/// threads, telemetry turns' request threads, and triggers telemetry named.
pub fn thread_candidates(snapshot: &Snapshot, channel: &str) -> BTreeSet<String> {
    let mut roots: BTreeSet<String> = snapshot
        .local
        .iter()
        .filter(|row| row.channel_id == channel)
        .map(|row| row.thread_root_id.clone())
        .collect();
    for turn in snapshot.turns.iter() {
        if turn.channel_id.as_deref() == Some(channel) {
            roots.extend(
                turn.requests
                    .iter()
                    .flatten()
                    .map(|request| request.thread_root_id.clone()),
            );
        }
    }
    roots.extend(
        snapshot
            .requested
            .iter()
            .filter(|(c, _)| c == channel)
            .map(|(_, id)| id.clone()),
    );
    roots
}

/// What a message's activity bubble shows (`messageActivity`): local work
/// until it is done, superseded or dismissed; telemetry while working or
/// unknown.
pub fn message_activity(snapshot: &Snapshot, channel: &str, message: &str) -> Vec<RequestWork> {
    request_work(snapshot, channel, &[message])
        .into_iter()
        .filter(|work| {
            if work.lifecycle {
                !matches!(work.state, WorkState::Done | WorkState::Superseded) && !work.dismissed
            } else {
                matches!(work.state, WorkState::Working | WorkState::Unknown)
            }
        })
        .collect()
}

/// Agents working in the thread `root` (`threadWorkingAgents`), from the
/// same request state as the bubbles. `represented` is a message whose
/// own bubble already shows these agents, so the thread summary leaves
/// them out unless they also work elsewhere in the thread. Typing counts
/// only for agents with no known request here.
pub fn thread_working_agents(
    snapshot: &Snapshot,
    typing: &[String],
    channel: &str,
    root: &str,
    represented: Option<(&str, &HashSet<String>)>,
) -> Vec<String> {
    let local = local_activity(&snapshot.local, channel, Some(root));
    let shown = |row: &LocalWork| {
        represented.is_some_and(|(message, agents)| {
            row.message_id == message && agents.contains(&row.agent)
        })
    };
    let active_local: Vec<String> = snapshot
        .local
        .iter()
        .filter(|row| {
            row.channel_id == channel
                && row.thread_root_id == root
                && row.state.active()
                && !shown(row)
        })
        .map(|row| row.agent.clone())
        .collect();
    let untracked: Vec<String> = typing
        .iter()
        .filter(|agent| !local.contains_key(agent.as_str()))
        .cloned()
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut push = |agent: String| {
        if !out.contains(&agent) {
            out.push(agent);
        }
    };
    if snapshot.status != FeedStatus::Listening {
        untracked
            .into_iter()
            .chain(active_local)
            .for_each(&mut push);
        return out;
    }
    let linked: Vec<&ActivityTurn> = snapshot
        .turns
        .iter()
        .filter(|turn| {
            turn.channel_id.as_deref() == Some(channel)
                && turn
                    .requests
                    .as_ref()
                    .is_some_and(|requests| requests.iter().any(|r| r.thread_root_id == root))
        })
        .collect();
    let mut requests: Vec<&str> = vec![root];
    for turn in &linked {
        for request in turn.requests.iter().flatten() {
            if request.thread_root_id == root {
                requests.push(&request.message_id);
            }
        }
    }
    let work = request_work(snapshot, channel, &requests);
    let elsewhere = match represented {
        Some((message, agents)) if !agents.is_empty() => {
            let others: Vec<&str> = requests
                .iter()
                .copied()
                .filter(|id| *id != message)
                .collect();
            request_work(snapshot, channel, &others)
        }
        _ => work.clone(),
    };
    let known: HashSet<&str> = linked
        .iter()
        .map(|turn| turn.agent.as_str())
        .chain(work.iter().map(|entry| entry.agent.as_str()))
        .collect();
    for agent in active_local {
        push(agent);
    }
    for entry in &work {
        if local.contains_key(entry.agent.as_str()) {
            continue;
        }
        let working = match represented {
            Some((_, agents)) if agents.contains(&entry.agent) => elsewhere
                .iter()
                .any(|other| other.agent == entry.agent && other.working),
            _ => entry.working,
        };
        if working {
            push(entry.agent.clone());
        }
    }
    for agent in untracked {
        if !known.contains(agent.as_str()) {
            push(agent);
        }
    }
    out
}

/// Whether every agent working in a thread is still only starting.
pub fn thread_waiting(
    snapshot: &Snapshot,
    agents: &[String],
    channel: &str,
    root: &str,
    represented: Option<(&str, &HashSet<String>)>,
) -> bool {
    let rows: Vec<LocalWork> = snapshot
        .local
        .iter()
        .filter(|row| {
            !represented.is_some_and(|(message, shown)| {
                row.message_id == message && shown.contains(&row.agent)
            })
        })
        .cloned()
        .collect();
    let local = local_activity(&rows, channel, Some(root));
    !agents.is_empty()
        && agents
            .iter()
            .all(|agent| breathing(local.get(agent.as_str()).copied()))
}

/// Agents whose work is tied to a message in this conversation: their
/// bubbles stand in for the composer's typing line
/// (`ActivityTypingReplacement`).
pub fn bubbled_agents(snapshot: &Snapshot, channel: &str, thread: Option<&str>) -> Vec<String> {
    let mut ids: Vec<&str> = thread.into_iter().collect();
    ids.extend(
        snapshot
            .local
            .iter()
            .filter(|row| {
                row.channel_id == channel && thread.is_none_or(|root| row.thread_root_id == root)
            })
            .map(|row| row.message_id.as_str()),
    );
    for turn in snapshot.turns.iter() {
        if turn.channel_id.as_deref() != Some(channel) {
            continue;
        }
        for request in turn.requests.iter().flatten() {
            if thread.is_none_or(|root| request.thread_root_id == root) {
                ids.push(&request.message_id);
            }
        }
    }
    request_work(snapshot, channel, &ids)
        .into_iter()
        .map(|work| work.agent)
        .collect()
}

/// A compact preview of a telemetry agent's live work: its latest tool,
/// else its latest thought, plan or reply (`currentActivity` of the
/// bundled activity plugin).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub title: String,
    pub detail: String,
}

pub fn preview(turns: &[TranscriptTurn], work: &RequestWork) -> Option<Preview> {
    let live: HashSet<&str> = work
        .turns
        .iter()
        .filter(|turn| turn.state == TurnState::Working)
        .map(|turn| turn.turn_id.as_str())
        .collect();
    let mut items: Vec<&super::transcript_model::Item> = turns
        .iter()
        .filter(|turn| live.contains(turn.turn_id.as_str()))
        .flat_map(|turn| turn.items.iter())
        .filter(|item| {
            matches!(
                item.kind,
                ItemKind::Tool(_)
                    | ItemKind::Thought { .. }
                    | ItemKind::Message { .. }
                    | ItemKind::Plan(_)
            )
        })
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.at));
    let latest = items
        .iter()
        .find(|item| matches!(item.kind, ItemKind::Tool(_)))
        .or_else(|| items.first())?;
    let (title, detail) = match &latest.kind {
        ItemKind::Tool(tool) => {
            let shown = presentation(&tool.title, &tool.input, &tool.output);
            let status = if shown.failed || tool.status == "failed" {
                " · Failed"
            } else {
                match tool.status.as_str() {
                    "completed" => " · Completed",
                    "pending" => " · Pending",
                    "in_progress" => " · Running",
                    _ => "",
                }
            };
            (
                format!("{}{status}", shown.title),
                shown.target.unwrap_or_default(),
            )
        }
        ItemKind::Thought { text, .. } => ("Thinking".into(), text.clone()),
        ItemKind::Plan(entries) => (
            "Plan".into(),
            entries
                .iter()
                .map(|entry| entry.content.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        ItemKind::Message { text, .. } => ("Response".into(), text.clone()),
        _ => return None,
    };
    let detail = if detail.chars().count() > 240 {
        format!("{}…", detail.chars().take(240).collect::<String>())
    } else {
        detail
    };
    Some(Preview { title, detail })
}

struct Presentation {
    title: String,
    target: Option<String>,
    failed: bool,
}

fn basename(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?;
    if value.trim().is_empty() || value.ends_with(['/', '\\']) || value.contains(['\r', '\n']) {
        return None;
    }
    let name = value.rsplit(['/', '\\']).next()?;
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

/// Display only: never feeds association, status or execution
/// (`activityPresentation`).
fn presentation(name: &str, input: &str, output: &str) -> Presentation {
    let input: Option<Value> = serde_json::from_str(input).ok().filter(Value::is_object);
    let mut shown = Presentation {
        title: name.to_string(),
        target: None,
        failed: false,
    };
    match name {
        "buzz-dev-mcp__shell" => {
            shown.title = "Run command".into();
            let line = input
                .as_ref()
                .and_then(|input| input.get("command"))
                .and_then(Value::as_str)
                .and_then(|command| command.lines().find(|line| !line.trim().is_empty()))
                .map(|line| {
                    line.chars()
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect::<String>()
                        .trim()
                        .to_string()
                });
            shown.target = line.map(|line| {
                let cut: String = line.chars().take(90).collect();
                if line.chars().count() > 90 {
                    format!("{cut}…")
                } else {
                    cut
                }
            });
            let result: Option<Value> = serde_json::from_str(output).ok();
            if let Some(result) = result.as_ref().filter(|r| {
                r.get("stdout").is_some_and(Value::is_string)
                    && r.get("stderr").is_some_and(Value::is_string)
                    && r.get("exit_code").is_some_and(|c| c.is_i64() || c.is_u64())
                    && r.get("timed_out").is_some_and(Value::is_boolean)
                    && r.get("stdout_truncated").is_some_and(Value::is_boolean)
                    && r.get("stderr_truncated").is_some_and(Value::is_boolean)
            }) {
                shown.failed = result.get("exit_code").and_then(Value::as_i64) != Some(0)
                    || result.get("timed_out").and_then(Value::as_bool) == Some(true);
            }
        }
        "buzz-dev-mcp__read_file" | "buzz-dev-mcp__str_replace" | "buzz-dev-mcp__view_image" => {
            shown.target = basename(input.as_ref().and_then(|input| input.get("path")));
            if shown.target.is_some() {
                shown.title = if name.ends_with("read_file") {
                    "Read file"
                } else if name.ends_with("str_replace") {
                    "Edit file"
                } else {
                    "View image"
                }
                .into();
            }
        }
        _ => {}
    }
    shown
}

#[cfg(test)]
mod tests;
