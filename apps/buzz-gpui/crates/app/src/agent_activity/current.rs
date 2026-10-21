//! What agents are doing right now, from live evidence only
//! (`current-activity.ts`, `working-agents.ts`, `activity-preview.ts`):
//! labels for the composer's working popover, the sidebar's working
//! badge and "Working now" rows, and the profile's latest activity.

use super::local::{local_activity, local_work_label};
use super::transcript_model::{ItemKind, Scope, ToolItem, TranscriptTurn, transcript};
use super::turns::{TurnState, parse_time};
use super::{Record, Snapshot, raw_records};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Weak};

/// Where an agent works: `Some(None)` is the channel conversation,
/// `Some(Some(root))` a thread, and `None` is genuinely unknown.
pub type Root = Option<Option<String>>;

/// An agent's live activity in one conversation (`CurrentActivity`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentActivity {
    pub label: String,
    pub roots: Vec<Root>,
}

fn verb(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "read" => "Reading",
        "edit" => "Editing",
        "delete" => "Deleting",
        "move" => "Moving",
        "search" => "Searching",
        "fetch" => "Fetching",
        "execute" => "Running a command",
        "think" => "Working",
        _ => return None,
    })
}

/// A compact label for one running tool (`toolLabel`). Never shows
/// arguments, commands or full paths.
pub fn tool_label(tool: &ToolItem) -> String {
    if tool
        .permission
        .as_ref()
        .is_some_and(|p| p.outcome.is_none())
    {
        return "Waiting for permission".into();
    }
    if tool.status == "pending" {
        return "Preparing a tool".into();
    }
    // These local MCP tools publish kind=other; only their exact names
    // are recognized.
    let local = match tool.title.as_str() {
        "buzz-dev-mcp__read_file" => Some("read"),
        "buzz-dev-mcp__str_replace" => Some("edit"),
        "buzz-dev-mcp__shell" => Some("execute"),
        _ => None,
    };
    let kind = local.unwrap_or(&tool.kind);
    let verb = verb(kind).unwrap_or("Running a tool");
    let mut path = tool.paths.first().cloned();
    if path.is_none()
        && local.is_some()
        && kind != "execute"
        && let Ok(input) = serde_json::from_str::<Value>(&tool.input)
    {
        path = input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let file = path.and_then(|path| {
        let name = path.rsplit(['/', '\\']).next().unwrap_or("").to_string();
        let clean: String = name
            .chars()
            .filter(|c| {
                let c = *c as u32;
                !(c <= 0x1f
                    || (0x7f..=0x9f).contains(&c)
                    || (0x202a..=0x202e).contains(&c)
                    || (0x2066..=0x2069).contains(&c))
            })
            .take(80)
            .collect();
        (!clean.is_empty()).then_some(clean)
    });
    match file {
        Some(file) if matches!(kind, "read" | "edit" | "delete" | "move") => {
            format!("{verb} {file}")
        }
        _ => verb.to_string(),
    }
}

/// Transcripts read for [`current_activity`]: (records, agent, channel).
type Read = (
    Weak<[Arc<Record>]>,
    String,
    String,
    Arc<Vec<TranscriptTurn>>,
);

thread_local! {
    static READ: RefCell<Vec<Read>> = const { RefCell::new(Vec::new()) };
}

/// Most transcripts kept; views ask for a few channels at a time.
const READ_LIMIT: usize = 32;

/// `agent`'s turns in `channel` from the snapshot's live records, read
/// once per set of records: views ask on every render, and the records
/// can hold megabytes of JSON.
pub(crate) fn live_turns(
    snapshot: &Snapshot,
    agent: &str,
    channel: &str,
) -> Arc<Vec<TranscriptTurn>> {
    let records = Arc::downgrade(&snapshot.records);
    let found = READ.with_borrow(|read| {
        read.iter()
            .find(|(seen, a, c, _)| seen.ptr_eq(&records) && a == agent && c == channel)
            .map(|(_, _, _, turns)| turns.clone())
    });
    if let Some(turns) = found {
        return turns;
    }
    let live: Vec<Arc<Record>> = snapshot
        .records
        .iter()
        .filter(|r| !r.historical)
        .cloned()
        .collect();
    let turns = Arc::new(
        transcript(
            &live,
            &Scope {
                agent: agent.to_string(),
                channel: Some(channel.to_string()),
                thread: None,
            },
        )
        .turns,
    );
    READ.with_borrow_mut(|read| {
        read.retain(|(seen, ..)| seen.strong_count() > 0);
        if read.len() >= READ_LIMIT {
            read.remove(0);
        }
        read.push((records, agent.into(), channel.into(), turns.clone()));
    });
    turns
}

/// A person typing: `(pubkey, channel, thread root)`.
pub type TypingEntry = (String, String, Option<String>);

/// Agents working in `channel` (or only in `thread`), with labels and
/// scopes (`currentActivity`). Channel views include threads.
pub fn current_activity(
    snapshot: Option<&Snapshot>,
    typing: &[TypingEntry],
    owned: &HashSet<String>,
    channel: &str,
    thread: Option<&str>,
) -> BTreeMap<String, CurrentActivity> {
    let mut agents: BTreeMap<String, CurrentActivity> = BTreeMap::new();
    let add = |agents: &mut BTreeMap<String, CurrentActivity>, agent: &str, root: Root| {
        let activity = agents
            .entry(agent.to_string())
            .or_insert_with(|| CurrentActivity {
                label: "Working".into(),
                roots: Vec::new(),
            });
        if !activity.roots.contains(&root) {
            activity.roots.push(root);
        }
    };
    let matches =
        |c: &str, root: Option<&str>| c == channel && (thread.is_none() || root == thread);
    let listening = snapshot.filter(|s| s.status == super::FeedStatus::Listening);
    let working: Vec<_> = listening
        .map(|s| {
            s.turns
                .iter()
                .filter(|t| {
                    t.channel_id.as_deref() == Some(channel) && t.state == TurnState::Working
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let owner_typing: Vec<_> = listening
        .map(|s| {
            s.typing
                .iter()
                .filter(|t| matches(&t.channel_id, t.thread_root.as_deref()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    for entry in &owner_typing {
        add(&mut agents, &entry.agent, Some(entry.thread_root.clone()));
    }
    for (pubkey, entry_channel, root) in typing {
        if matches(entry_channel, root.as_deref())
            && (owned.contains(pubkey)
                || owner_typing.iter().any(|o| o.agent == *pubkey)
                || working.iter().any(|t| t.agent == *pubkey))
        {
            add(&mut agents, pubkey, Some(root.clone()));
        }
    }
    let Some(snapshot) = listening else {
        return match snapshot {
            Some(snapshot) => with_local(agents, snapshot, channel, thread),
            None => agents,
        };
    };
    let mut seen: Vec<&str> = working.iter().map(|t| t.agent.as_str()).collect();
    seen.sort();
    seen.dedup();
    for agent in seen {
        let turns = live_turns(snapshot, agent, channel);
        let mut tools: Vec<ToolItem> = Vec::new();
        for evidence in working.iter().filter(|t| t.agent == agent) {
            let turn = turns.iter().find(|t| t.turn_id == evidence.turn_id);
            if turn.is_some_and(|t| t.ended_at.is_some())
                || (thread.is_some()
                    && turn
                        .and_then(|t| t.thread_root.clone())
                        .flatten()
                        .as_deref()
                        != thread)
            {
                continue;
            }
            add(&mut agents, agent, turn.and_then(|t| t.thread_root.clone()));
            if let Some(turn) = turn {
                tools.extend(turn.items.iter().filter_map(|item| match &item.kind {
                    ItemKind::Tool(tool)
                        if (!tool.title.is_empty() || !tool.kind.is_empty())
                            && matches!(tool.status.as_str(), "in_progress" | "pending") =>
                    {
                        Some(tool.clone())
                    }
                    _ => None,
                }));
            }
        }
        if let (Some(activity), Some(first)) = (agents.get_mut(agent), tools.first()) {
            activity.label = if tools.len() > 1 {
                format!("Running {} tools", tools.len())
            } else {
                tool_label(first)
            };
        }
    }
    with_local(agents, snapshot, channel, thread)
}

/// This app's own agents' work replaces their other evidence here: shown
/// with its own label while to come or going on, and not at all once
/// settled.
fn with_local(
    mut agents: BTreeMap<String, CurrentActivity>,
    snapshot: &Snapshot,
    channel: &str,
    thread: Option<&str>,
) -> BTreeMap<String, CurrentActivity> {
    for (agent, work) in local_activity(&snapshot.local, channel, thread) {
        agents.remove(agent);
        if !work.state.active() {
            continue;
        }
        let mut roots: Vec<Root> = Vec::new();
        for row in snapshot.local.iter().filter(|row| {
            row.agent == agent
                && row.channel_id == channel
                && thread.is_none_or(|root| row.thread_root_id == root)
                && row.state.active()
        }) {
            let root = Some(Some(row.thread_root_id.clone()));
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        agents.insert(
            agent.to_string(),
            CurrentActivity {
                label: local_work_label(work.state, work.error.as_deref(), work.step.as_ref()),
                roots,
            },
        );
    }
    agents
}

/// Agents working in `channel` at its top level or by observed turns
/// (`workingAgents`). A thread typing signal stays in its thread.
pub fn working_agents(snapshot: &Snapshot, channel: &str) -> Vec<String> {
    let local = local_activity(&snapshot.local, channel, None);
    let mut agents: Vec<String> = local
        .values()
        .filter(|row| row.state.active())
        .map(|row| row.agent.clone())
        .collect();
    if snapshot.status == super::FeedStatus::Listening {
        agents.extend(
            snapshot
                .turns
                .iter()
                .filter(|t| {
                    t.channel_id.as_deref() == Some(channel)
                        && t.state == TurnState::Working
                        && !local.contains_key(t.agent.as_str())
                })
                .map(|t| t.agent.clone())
                .chain(
                    snapshot
                        .typing
                        .iter()
                        .filter(|t| {
                            t.channel_id == channel
                                && t.thread_root.is_none()
                                && !local.contains_key(t.agent.as_str())
                        })
                        .map(|t| t.agent.clone()),
                ),
        );
    }
    agents.sort();
    agents.dedup();
    agents
}

/// When an agent's current work started, and the message that started it
/// when exactly one is known (`workingAgentDetails`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingDetails {
    pub started_at: u64,
    pub message_id: Option<String>,
    /// Local work's own line ("Starting", "Reading main.rs").
    pub label: Option<String>,
    /// Reported by this app's runtime: no telemetry to open.
    pub local: bool,
}

pub fn working_details(snapshot: &Snapshot, channel: &str, agent: &str) -> Option<WorkingDetails> {
    if let Some(work) = local_activity(&snapshot.local, channel, None).get(agent) {
        if !work.state.active() {
            return None;
        }
        let active: Vec<_> = snapshot
            .local
            .iter()
            .filter(|row| row.channel_id == channel && row.agent == agent && row.state.active())
            .collect();
        return Some(WorkingDetails {
            started_at: active
                .iter()
                .map(|row| row.started_at)
                .min()
                .unwrap_or(work.started_at),
            message_id: (active.len() == 1).then(|| work.message_id.clone()),
            label: Some(local_work_label(
                work.state,
                work.error.as_deref(),
                work.step.as_ref(),
            )),
            local: true,
        });
    }
    if snapshot.status != super::FeedStatus::Listening {
        return None;
    }
    let typing = snapshot
        .typing
        .iter()
        .find(|t| t.agent == agent && t.channel_id == channel && t.thread_root.is_none())
        .map(|t| WorkingDetails {
            started_at: t.started_at,
            message_id: None,
            label: None,
            local: false,
        });
    let mut turns: Vec<_> = snapshot
        .turns
        .iter()
        .filter(|t| {
            t.agent == agent
                && t.channel_id.as_deref() == Some(channel)
                && t.state == TurnState::Working
        })
        .collect();
    turns.sort_by_key(|turn| std::cmp::Reverse(turn.timestamp));
    let Some(latest) = turns.first() else {
        return typing;
    };
    if turns
        .get(1)
        .is_some_and(|t| t.timestamp == latest.timestamp)
    {
        return None;
    }
    let starts: Vec<(String, u64, Option<String>)> =
        raw_records(&snapshot.records, agent, Some(channel))
            .into_iter()
            .filter(|r| r.kind == "turn_started")
            .filter_map(|r| {
                let value: Value = serde_json::from_str(&r.plaintext).ok()?;
                let turn = value.get("turnId")?.as_str()?.to_string();
                let at = parse_time(value.get("timestamp")?.as_str()?)
                    .filter(|at| *at <= r.received_at + 5000)?;
                let ids = value
                    .get("payload")
                    .and_then(|p| p.get("triggeringEventIds"))
                    .and_then(Value::as_array);
                let message = ids
                    .filter(|ids| ids.len() == 1)
                    .and_then(|ids| ids[0].as_str())
                    .filter(|id| super::hex64(id))
                    .map(str::to_string);
                Some((turn, at, message))
            })
            .collect();
    let matching: Vec<_> = starts
        .iter()
        .filter(|(turn, ..)| *turn == latest.turn_id)
        .collect();
    match matching.as_slice() {
        [(_, started_at, message_id)] => Some(WorkingDetails {
            started_at: *started_at,
            message_id: message_id.clone(),
            label: None,
            local: false,
        }),
        _ => typing,
    }
}

/// `12s`, `3m 4s`, `1h 2m 3s` (`workingDuration`).
pub fn working_duration(started_at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(started_at) / 1000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m {}s", seconds % 60);
    }
    format!("{}h {}m {}s", minutes / 60, minutes % 60, seconds % 60)
}

/// One line of the profile's latest activity (`activityPreview`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewItem {
    pub id: String,
    pub label: String,
    pub text: String,
    pub timestamp: u64,
}

/// The last 600 characters, after an ellipsis.
fn clipped(value: &str) -> String {
    let count = value.chars().count();
    if count <= 600 {
        return value.to_string();
    }
    let tail: String = value.chars().skip(count - 600).collect();
    format!("…{tail}")
}

/// The three most recently updated reply and tool lines of `agent` in
/// `channel`. Prompts, thoughts, arguments and results stay out.
pub fn activity_preview(records: &[Arc<Record>], agent: &str, channel: &str) -> Vec<PreviewItem> {
    if channel.is_empty() {
        return Vec::new();
    }
    let mut items: Vec<PreviewItem> = Vec::new();
    let mut segments: BTreeMap<String, u64> = BTreeMap::new();
    for record in records.iter().filter(|r| r.agent == agent) {
        let Ok(raw) = serde_json::from_str::<Value>(&record.plaintext) else {
            continue;
        };
        for (index, event) in super::children(&raw).into_iter().enumerate() {
            if event.get("channelId").and_then(Value::as_str) != Some(channel) {
                continue;
            }
            let payload = event.get("payload");
            if event.get("kind").and_then(Value::as_str) != Some("acp_read")
                || payload
                    .and_then(|p| p.get("method"))
                    .and_then(Value::as_str)
                    != Some("session/update")
            {
                continue;
            }
            let Some(update) = payload
                .and_then(|p| p.get("params"))
                .and_then(|p| p.get("update"))
            else {
                continue;
            };
            let field = |value: &Value, key: &str| {
                value
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let scope = [field(event, "sessionId"), field(event, "turnId")];
            let scope_key = serde_json::to_string(&scope).unwrap_or_default();
            let timestamp = parse_time(&field(event, "timestamp"))
                .filter(|at| *at <= record.received_at + 5000)
                .unwrap_or(record.received_at);
            let kind = field(update, "sessionUpdate");
            let (id, label, content) = if kind == "agent_message_chunk" {
                let block = update.get("content").cloned().unwrap_or(Value::Null);
                let text = field(&block, "text");
                if field(&block, "type") != "text" || text.is_empty() {
                    continue;
                }
                // Without a message or turn identity, don't join chunks.
                let message = field(update, "messageId");
                let turn = field(event, "turnId");
                let id = serde_json::json!([
                    scope[0],
                    scope[1],
                    "message",
                    message,
                    if message.is_empty() {
                        if turn.is_empty() {
                            format!("{}:{index}", record.id)
                        } else {
                            turn
                        }
                    } else {
                        String::new()
                    },
                    if message.is_empty() {
                        segments.get(&scope_key).copied().unwrap_or(0)
                    } else {
                        0
                    },
                ])
                .to_string();
                let previous = items
                    .iter()
                    .find(|i| i.id == id)
                    .map(|i| i.text.clone())
                    .unwrap_or_default();
                (id, "Assistant".to_string(), clipped(&(previous + &text)))
            } else if kind == "tool_call" || kind == "tool_call_update" {
                let call = field(update, "toolCallId");
                if call.is_empty() {
                    continue;
                }
                // A tool seals this turn's unkeyed message.
                *segments.entry(scope_key.clone()).or_insert(0) += 1;
                let id = serde_json::json!([scope[0], scope[1], "tool", call]).to_string();
                let previous = items.iter().find(|i| i.id == id).cloned();
                let title = field(update, "title");
                let content = clipped(if !title.is_empty() {
                    &title
                } else if let Some(previous) = &previous {
                    &previous.text
                } else {
                    "Tool call"
                });
                let label = match field(update, "status").as_str() {
                    "completed" => "Tool completed".to_string(),
                    "failed" => "Tool error".to_string(),
                    "in_progress" => "Using tool".to_string(),
                    "pending" => "Tool pending".to_string(),
                    _ => previous
                        .map(|p| p.label)
                        .filter(|l| !l.is_empty())
                        .unwrap_or_else(|| "Tool".into()),
                };
                (id, label, content)
            } else {
                continue;
            };
            items.retain(|item| item.id != id);
            items.push(PreviewItem {
                id,
                label,
                text: content,
                timestamp,
            });
        }
    }
    let skip = items.len().saturating_sub(3);
    items.into_iter().skip(skip).collect()
}

/// The viewer's agents: every agent on a connected host, plus agents
/// whose verified owner attestation names the viewer (`agentChoices`).
pub fn owned_agents(
    hosts: impl IntoIterator<Item = String>,
    profiles: &buzz_relay_client::Profiles,
    viewer: &str,
) -> HashSet<String> {
    let mut owned: HashSet<String> = hosts.into_iter().collect();
    owned.extend(
        profiles
            .iter()
            .filter(|(_, p)| p.attested_owner.as_deref() == Some(viewer))
            .map(|(key, _)| key.clone()),
    );
    owned
}

/// Who types where, flattened from the session's typing maps. A thread
/// typer's channel is the one whose "anywhere" list has them but whose
/// top level does not.
pub fn typing_entries(snapshot: &buzz_relay_client::SessionSnapshot) -> Vec<TypingEntry> {
    let mut entries: Vec<TypingEntry> = Vec::new();
    for (channel, people) in snapshot.typing.iter() {
        for person in people {
            entries.push((person.clone(), channel.clone(), None));
        }
    }
    for (root, people) in snapshot.thread_typing.iter() {
        for person in people {
            let channel = snapshot.typing_anywhere.iter().find(|(channel, anywhere)| {
                anywhere.contains(person)
                    && !snapshot
                        .typing
                        .get(*channel)
                        .is_some_and(|top| top.contains(person))
            });
            if let Some((channel, _)) = channel {
                entries.push((person.clone(), channel.clone(), Some(root.clone())));
            }
        }
    }
    entries.sort();
    entries
}

#[cfg(test)]
mod tests {
    use super::super::transcript_model::Permission;
    use super::super::turns::{ActivityTurn, Typing};
    use super::super::{FeedStatus, Record};
    use super::*;
    use serde_json::json;

    fn tool(kind: &str, status: &str, title: &str, paths: &[&str]) -> ToolItem {
        ToolItem {
            tool_call_id: "x".into(),
            title: title.into(),
            kind: kind.into(),
            status: status.into(),
            input: String::new(),
            output: String::new(),
            diffs: Vec::new(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            completed_at: None,
            truncated: false,
            permission: None,
        }
    }

    #[test]
    fn tool_labels_follow_the_reference() {
        assert_eq!(
            tool_label(&tool("read", "in_progress", "Read", &["/a/b/main.rs"])),
            "Reading main.rs"
        );
        assert_eq!(
            tool_label(&tool("execute", "in_progress", "ls", &["/x"])),
            "Running a command"
        );
        assert_eq!(
            tool_label(&tool("other", "in_progress", "x", &[])),
            "Running a tool"
        );
        assert_eq!(
            tool_label(&tool("read", "pending", "x", &[])),
            "Preparing a tool"
        );
        let mut local = tool("other", "in_progress", "buzz-dev-mcp__read_file", &[]);
        local.input = json!({"path": "src/lib\u{202e}.rs"}).to_string();
        assert_eq!(tool_label(&local), "Reading lib.rs");
        let mut asking = tool("edit", "in_progress", "Edit", &[]);
        asking.permission = Some(Permission {
            options: vec!["Allow".into()],
            outcome: None,
        });
        assert_eq!(tool_label(&asking), "Waiting for permission");
    }

    const NOW: u64 = 1_791_633_610_000;
    const AGENT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn listening(
        turns: Vec<ActivityTurn>,
        typing: Vec<Typing>,
        records: Vec<Arc<Record>>,
    ) -> Snapshot {
        Snapshot {
            status: FeedStatus::Listening,
            turns: turns.into(),
            typing: typing.into(),
            records: records.into(),
            ..Default::default()
        }
    }

    fn record(plaintext: Value) -> Arc<Record> {
        Arc::new(Record {
            id: "r".into(),
            agent: AGENT.into(),
            created_at: NOW / 1000,
            plaintext: plaintext.to_string().into(),
            received_at: NOW,
            historical: false,
            kind: plaintext["kind"].as_str().unwrap_or("unknown").into(),
            channel_ids: vec!["c1".into()],
        })
    }

    fn working(turn: &str) -> ActivityTurn {
        ActivityTurn {
            agent: AGENT.into(),
            turn_id: turn.into(),
            channel_id: Some("c1".into()),
            timestamp: NOW,
            state: TurnState::Working,
            requests: None,
        }
    }

    #[test]
    fn current_activity_labels_running_tools() {
        let at = "2026-10-10T12:00:00Z";
        let snapshot = listening(
            vec![working("t1")],
            Vec::new(),
            vec![record(json!({"kind": "batch", "payload": {"events": [
                {"kind": "turn_started", "turnId": "t1", "channelId": "c1", "timestamp": at, "payload": {"threadRootEventId": "b".repeat(64)}},
                {"kind": "acp_read", "turnId": "t1", "channelId": "c1", "timestamp": at, "payload": {"method": "session/update", "params": {"update": {"sessionUpdate": "tool_call", "toolCallId": "1", "title": "Read", "kind": "read", "status": "in_progress", "locations": [{"path": "/x/a.rs"}]}}}},
            ]}}))],
        );
        let owned = HashSet::new();
        let found = current_activity(Some(&snapshot), &[], &owned, "c1", None);
        let activity = &found[AGENT];
        assert_eq!(activity.label, "Reading a.rs");
        assert_eq!(activity.roots, vec![Some(Some("b".repeat(64)))]);
        // Not listening: nothing is current.
        let quiet = Snapshot {
            status: FeedStatus::Interrupted,
            ..snapshot.clone()
        };
        assert!(current_activity(Some(&quiet), &[], &owned, "c1", None).is_empty());
        // Another thread's view does not show it.
        assert!(
            current_activity(Some(&snapshot), &[], &owned, "c1", Some(&"c".repeat(64))).is_empty()
        );
    }

    #[test]
    fn owned_typing_counts_without_telemetry() {
        let owned: HashSet<String> = [AGENT.to_string()].into();
        let typing = vec![(AGENT.to_string(), "c1".to_string(), None)];
        let found = current_activity(None, &typing, &owned, "c1", None);
        assert_eq!(found[AGENT].label, "Working");
        assert_eq!(found[AGENT].roots, vec![Some(None)]);
        // Someone else's agent typing is just typing.
        assert!(current_activity(None, &typing, &HashSet::new(), "c1", None).is_empty());
    }

    #[test]
    fn working_agents_and_details() {
        let message = "d".repeat(64);
        let snapshot = listening(
            vec![working("t1")],
            vec![Typing {
                agent: "e".repeat(64),
                channel_id: "c1".into(),
                thread_root: None,
                started_at: NOW - 5000,
                expires_at: NOW + 3000,
            }],
            vec![record(
                json!({"kind": "turn_started", "turnId": "t1", "channelId": "c1", "timestamp": "2026-10-10T12:00:00Z", "payload": {"triggeringEventIds": [message]}}),
            )],
        );
        assert_eq!(
            working_agents(&snapshot, "c1"),
            vec![AGENT.to_string(), "e".repeat(64)]
        );
        let details = working_details(&snapshot, "c1", AGENT).unwrap();
        assert_eq!(details.message_id, Some(message));
        assert_eq!(
            details.started_at,
            parse_time("2026-10-10T12:00:00Z").unwrap()
        );
        assert_eq!(
            working_details(&snapshot, "c1", &"e".repeat(64))
                .unwrap()
                .started_at,
            NOW - 5000
        );
        assert_eq!(working_duration(0, 5_000), "5s");
        assert_eq!(working_duration(0, 125_000), "2m 5s");
        assert_eq!(working_duration(0, 3_725_000), "1h 2m 5s");
    }

    #[test]
    fn preview_keeps_the_last_three_reply_and_tool_lines() {
        let at = "2026-10-10T12:00:00Z";
        let update = |update: Value| json!({"kind": "acp_read", "turnId": "t1", "sessionId": "s", "channelId": "c1", "timestamp": at, "payload": {"method": "session/update", "params": {"update": update}}});
        let records = vec![record(json!({"kind": "batch", "payload": {"events": [
            update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hel"}})),
            update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "lo"}})),
            update(json!({"sessionUpdate": "tool_call", "toolCallId": "1", "title": "Read a.rs", "status": "in_progress"})),
            update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "after"}})),
            update(json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hidden"}})),
            update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "status": "completed"})),
        ]}}))];
        let items = activity_preview(&records, AGENT, "c1");
        let shown: Vec<(&str, &str)> = items
            .iter()
            .map(|i| (i.label.as_str(), i.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("Assistant", "Hello"),
                ("Assistant", "after"),
                ("Tool completed", "Read a.rs")
            ]
        );
        assert!(activity_preview(&records, AGENT, "").is_empty());
        assert_eq!(clipped(&"x".repeat(700)).chars().count(), 601);
    }
}
