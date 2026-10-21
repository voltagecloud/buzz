//! A conversation's Claude session, read from the transcript Claude keeps
//! on the agent's host and shown in the activity transcript model
//! (buzz-app's `transcript.ts`). Claude owns the file; nothing here writes
//! or keeps it.

use crate::agent_activity::transcript_model::{
    Item, ItemKind, ToolItem, Transcript, TranscriptTurn, prompt_message, prompt_sections,
};
use crate::agent_activity::turns::parse_time;
use crate::agent_plugins::process::{SpawnOptions, Spawner, collect};
use crate::buzz_mcp::app_client::{Signal, read_file};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// The latest turns shown; the file keeps the rest.
pub const TURN_LIMIT: usize = 50;
const STEER: &str = "new-message-arrived-while-you-were-working";
/// Longer project names get a hash suffix this does not reproduce.
const PROJECT_NAME_LIMIT: usize = 200;

/// One session file's conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub session_id: String,
    pub channel_id: String,
    pub root: Option<String>,
}

/// A conversation to read: the agent's sessions for its thread or
/// channel, including the one this app saved, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    /// The agent's pubkey, as its turn prompts name it.
    pub agent: String,
    pub session_id: Option<String>,
    pub channel_id: String,
    pub root: Option<String>,
}

/// The latest turns of a read, and whether the file has earlier ones.
#[derive(Clone, Debug, PartialEq)]
pub struct Read {
    pub transcript: Transcript,
    pub more: bool,
}

fn str_of(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or("")
}

/// JavaScript's truthiness, for flags Claude writes.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

fn blocks(value: Option<&Value>) -> Vec<&Map<String, Value>> {
    value
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default()
}

/// Message content as text: a string, or its text blocks joined.
fn text(value: Option<&Value>) -> String {
    if let Some(text) = value.and_then(Value::as_str) {
        return text.to_string();
    }
    blocks(value)
        .iter()
        .map(|block| str_of(block.get("text")))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Claude's tools by ACP tool kind.
fn kind(name: &str) -> &'static str {
    match name {
        "Bash" => "execute",
        "Read" => "read",
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => "edit",
        "Grep" | "Glob" => "search",
        "WebFetch" | "WebSearch" => "fetch",
        _ => "other",
    }
}

/// MCP tools as Codex names them: `buzz.read`, not `mcp__buzz__read`.
fn buzz_name(name: &str) -> String {
    name.strip_prefix("mcp__")
        .and_then(|rest| {
            let (server, tool) = rest.split_once("__")?;
            (!server.is_empty()).then(|| format!("{server}.{tool}"))
        })
        .unwrap_or_else(|| name.into())
}

/// A Buzz prompt, or a steer wrapping one, as the turn's opening item.
fn prompt(id: &str, at: u64, raw: &str) -> Item {
    let mut sections = prompt_sections(raw);
    let steer = sections
        .iter()
        .find(|section| section.tag == STEER)
        .map(|section| section.body.clone());
    if let Some(body) = &steer {
        sections = prompt_sections(body);
    }
    let (text, author) = prompt_message(&sections);
    Item {
        id: id.into(),
        at,
        kind: ItemKind::Prompt {
            text: if text.is_empty() { raw.into() } else { text },
            author,
            sections,
            steer: steer.is_some(),
        },
    }
}

/// Transcript lines as turns, each opened by a prompt. Claude's own
/// entries (metadata, subagents, attachments) are left out.
pub fn claude_turns(file: &str, conversation: &Session, working: bool) -> Vec<TranscriptTurn> {
    let mut turns: Vec<TranscriptTurn> = Vec::new();
    // Tool calls by id: their turn and item.
    let mut tools: HashMap<String, (usize, usize)> = HashMap::new();
    // Messages sent with `send`, until their result says whether they
    // were posted: their turn and item.
    let mut sends: HashMap<String, (usize, usize)> = HashMap::new();
    let mut last = 0;
    let start = |turns: &mut Vec<TranscriptTurn>, last: u64, id: &str, at: u64, partial: bool| {
        if let Some(current) = turns.last_mut() {
            current.ended_at = Some(last.max(current.started_at));
        }
        turns.push(TranscriptTurn {
            turn_id: id.into(),
            channel_id: Some(conversation.channel_id.clone()),
            thread_root: Some(conversation.root.clone()),
            source: None,
            triggering_event_ids: Vec::new(),
            session_id: Some(conversation.session_id.clone()),
            new_session: false,
            config: Vec::new(),
            started_at: at,
            ended_at: None,
            error: None,
            stop_reason: None,
            context: None,
            partial,
            items: Vec::new(),
        });
    };
    for line in file.split('\n') {
        let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if truthy(entry.get("isSidechain")) || truthy(entry.get("isMeta")) {
            continue;
        }
        let message = entry.get("message").and_then(Value::as_object);
        let id = str_of(entry.get("uuid"));
        let at = parse_time(str_of(entry.get("timestamp")))
            .filter(|at| *at != 0)
            .unwrap_or(last);
        let kind_of = str_of(entry.get("type"));
        let Some(message) = message else { continue };
        if id.is_empty() || (kind_of != "user" && kind_of != "assistant") {
            continue;
        }
        let content = message.get("content");
        let results: Vec<_> = blocks(content)
            .into_iter()
            .filter(|block| str_of(block.get("type")) == "tool_result")
            .collect();
        if kind_of == "user" && results.is_empty() {
            let item = prompt(id, at, &text(content));
            let steer = matches!(item.kind, ItemKind::Prompt { steer: true, .. });
            if !steer || turns.is_empty() {
                start(&mut turns, last, id, at, false);
            }
            if let Some(current) = turns.last_mut() {
                current.items.push(item);
            }
            last = at;
            continue;
        }
        if turns.is_empty() {
            start(&mut turns, last, id, at, true);
        }
        last = at;
        for result in results {
            let call = str_of(result.get("tool_use_id"));
            if let Some(&(turn, index)) = sends.get(call) {
                if !truthy(result.get("is_error")) {
                    if let ItemKind::Message { posted, .. } = &mut turns[turn].items[index].kind {
                        *posted = true;
                    }
                } else {
                    turns[turn].items.push(Item {
                        id: format!("{id}:{call}"),
                        at,
                        kind: ItemKind::Status(format!(
                            "The message above was not posted: {}",
                            text(result.get("content"))
                        )),
                    });
                }
            }
            let Some(&(turn, index)) = tools.get(call) else {
                continue;
            };
            if let ItemKind::Tool(tool) = &mut turns[turn].items[index].kind {
                tool.output = text(result.get("content"));
                tool.status = if truthy(result.get("is_error")) {
                    "failed"
                } else {
                    "completed"
                }
                .into();
                tool.completed_at = Some(at);
            }
        }
        let turn = turns.len() - 1;
        for (index, block) in blocks(content).into_iter().enumerate() {
            let key = format!("{id}:{index}");
            let item = match str_of(block.get("type")) {
                "text" if !str_of(block.get("text")).trim().is_empty() => ItemKind::Message {
                    text: str_of(block.get("text")).into(),
                    message_id: None,
                    posted: false,
                },
                "thinking" if !str_of(block.get("thinking")).trim().is_empty() => {
                    ItemKind::Thought {
                        text: str_of(block.get("thinking")).into(),
                        message_id: None,
                    }
                }
                "tool_use" => {
                    let name = str_of(block.get("name"));
                    let input = block
                        .get("input")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let sent = str_of(input.get("text"));
                    if name == "mcp__buzz__send" && !sent.trim().is_empty() {
                        sends.insert(
                            str_of(block.get("id")).to_string(),
                            (turn, turns[turn].items.len()),
                        );
                        turns[turn].items.push(Item {
                            id: key,
                            at,
                            kind: ItemKind::Message {
                                text: sent.into(),
                                message_id: None,
                                posted: false,
                            },
                        });
                        continue;
                    }
                    let path = Some(str_of(input.get("file_path")))
                        .filter(|path| !path.is_empty())
                        .unwrap_or(str_of(input.get("path")));
                    let command = str_of(input.get("command"));
                    let call = str_of(block.get("id")).to_string();
                    tools.insert(call.clone(), (turn, turns[turn].items.len()));
                    ItemKind::Tool(ToolItem {
                        tool_call_id: call,
                        title: if command.is_empty() {
                            buzz_name(name)
                        } else {
                            command.into()
                        },
                        kind: kind(name).into(),
                        status: "in_progress".into(),
                        input: serde_json::to_string_pretty(&Value::Object(input.clone()))
                            .unwrap_or_default(),
                        output: String::new(),
                        diffs: Vec::new(),
                        paths: if path.is_empty() {
                            Vec::new()
                        } else {
                            vec![path.into()]
                        },
                        completed_at: None,
                        truncated: false,
                        permission: None,
                    })
                }
                _ => continue,
            };
            turns[turn].items.push(Item {
                id: key,
                at,
                kind: item,
            });
        }
    }
    if let Some(current) = turns.last_mut()
        && !working
    {
        current.ended_at = Some(last.max(current.started_at));
    }
    turns
}

/// Claude's project folder name for a working directory.
pub fn project_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Whether a session file was started for `conversation`: its first
/// prompt names the same agent, channel and, for a thread session, thread
/// root. Prompts from before agents were named match any agent.
pub fn started_for(file: &str, conversation: &Conversation) -> bool {
    for line in file.split('\n') {
        let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if str_of(entry.get("type")) != "user"
            || truthy(entry.get("isMeta"))
            || truthy(entry.get("isSidechain"))
        {
            continue;
        }
        let content = entry
            .get("message")
            .and_then(|message| message.get("content"));
        let Some(context) = prompt_sections(&text(content))
            .into_iter()
            .find(|section| section.tag == "context")
        else {
            return false;
        };
        let field = |name: &str| {
            context
                .body
                .split('\n')
                .find_map(|line| line.strip_prefix(&format!("{name}: ")))
                .map(str::to_string)
        };
        let agent = field("Agent");
        let scope = field("Session scope");
        return agent.is_none_or(|agent| agent == conversation.agent)
            && field("Channel").is_some_and(|channel| {
                channel.ends_with(&format!("(#{})", conversation.channel_id))
            })
            && match &conversation.root {
                Some(root) => {
                    scope.as_deref() == Some("thread")
                        && field("Thread root").as_ref() == Some(root)
                }
                None => scope.as_deref() != Some("thread"),
            };
    }
    false
}

/// Other session files mentioning `pattern`, by name. The agent starts a
/// new session when it cannot resume one, e.g. in another copy of the app,
/// which keeps its own saved sessions.
async fn other_sessions(spawn: &Spawner, project: &str, pattern: &str) -> Vec<String> {
    let spawned = spawn
        .spawn(
            "find",
            SpawnOptions {
                args: vec![pattern.into(), ".".into()],
                cwd: Some(format!("~/.claude/projects/{project}")),
                ..Default::default()
            },
        )
        .await;
    // No match exits 1; a failed search leaves just the saved session.
    let Ok(mut spawned) = spawned else {
        return Vec::new();
    };
    let (_, output, _) = collect(&mut spawned.output).await;
    output
        .split('\n')
        .map(|line| {
            let line = line.trim();
            line.strip_prefix("./").unwrap_or(line).to_string()
        })
        .filter(|name| name.ends_with(".jsonl") && name.len() > 6 && !name.contains('/'))
        .collect()
}

/// The conversation's transcript across its sessions, oldest turn first,
/// from the folder Claude keeps for the agent's workspace, or `None` when
/// the host has no file for it: deleted, or started in a workspace the
/// agent no longer uses.
pub async fn read_transcript(
    spawn: &Spawner,
    workspace: &str,
    conversation: &Conversation,
    working: bool,
    signal: &Signal,
) -> Result<Option<Read>, String> {
    let mut spawned = spawn
        .spawn(
            "workspace",
            SpawnOptions {
                cwd: Some(workspace.into()),
                ..Default::default()
            },
        )
        .await?;
    let (code, directory, _) = collect(&mut spawned.output).await;
    if code != Some(0) {
        return Err("Could not find the agent's workspace".into());
    }
    signal.check()?;
    let project = project_name(directory.trim());
    if project.len() > PROJECT_NAME_LIMIT {
        return Ok(None);
    }
    let saved = conversation
        .session_id
        .as_ref()
        .map(|id| format!("{id}.jsonl"));
    let pattern = conversation
        .root
        .as_deref()
        .unwrap_or(&conversation.channel_id);
    let mut names: Vec<String> = saved.iter().cloned().collect();
    for name in other_sessions(spawn, &project, pattern).await {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    signal.check()?;
    let mut turns = Vec::new();
    let mut found = false;
    for name in names {
        // Project names start with `-`; base64 would read one as options.
        let path = format!("./{project}/{name}");
        let file = match read_file(spawn, "~/.claude/projects", &path, Some(signal)).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                signal.check()?;
                if error.to_lowercase().contains("no such file") {
                    continue;
                }
                return Err(error);
            }
        };
        let own = saved.as_ref() == Some(&name);
        if !own && !started_for(&file, conversation) {
            continue;
        }
        found = true;
        let session = Session {
            session_id: name.trim_end_matches(".jsonl").into(),
            channel_id: conversation.channel_id.clone(),
            root: conversation.root.clone(),
        };
        turns.extend(claude_turns(&file, &session, working && own));
    }
    if !found {
        return Ok(None);
    }
    turns.sort_by_key(|turn| turn.started_at);
    let more = turns.len() > TURN_LIMIT;
    turns.drain(..turns.len().saturating_sub(TURN_LIMIT));
    Ok(Some(Read {
        transcript: Transcript {
            turns,
            unknown_thread: 0,
        },
        more,
    }))
}

#[cfg(test)]
mod tests;
