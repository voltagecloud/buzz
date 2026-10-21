//! A conversation's Codex thread, read back through a short-lived
//! `codex app-server` and shown in the activity transcript model
//! (buzz-app's `transcript.ts`). Codex owns the history; nothing here is
//! saved.

use super::prompt::{Conversation, session_tag, session_title};
use super::rpc::AppServer;
use crate::agent_activity::transcript_model::{
    Item, ItemKind, ToolItem, Transcript, TranscriptTurn, prompt_sections,
};
pub use crate::agent_plugins::opened::Place;
use crate::agent_plugins::process::Spawner;
use crate::buzz_mcp::app_client::Signal;
use serde_json::{Map, Value, json};

/// The latest turns shown; earlier ones stay in Codex's own history.
pub const TURN_LIMIT: usize = 20;

/// A thread's latest turns, whether it has earlier ones, and whether its
/// latest turn is still running.
#[derive(Clone, Debug, PartialEq)]
pub struct CodexRead {
    pub transcript: Transcript,
    pub more: bool,
    pub working: bool,
}

/// A display name for a pubkey, if known.
pub type Names<'a> = &'a (dyn Fn(&str) -> Option<String> + Send + Sync);
/// [`Names`] that a reader keeps.
pub type SharedNames = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// A conversation to read: its saved thread, if this app has one, and
/// what finds the threads other copies of the app started for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lookup {
    pub agent_pubkey: String,
    pub agent_name: String,
    pub channel_id: String,
    /// The channel's name, as the agent named threads with it.
    pub name: Option<String>,
    pub root: Option<String>,
    pub thread_id: Option<String>,
}

impl Lookup {
    fn place(&self) -> Place {
        Place {
            channel_id: self.channel_id.clone(),
            root: self.root.clone(),
        }
    }
}

/// Codex runs each command through the login shell, as
/// `/bin/zsh -lc '…'`; the command the model wrote is the quoted part.
pub fn shell_command(command: &str) -> String {
    let quoted = command.split_once(" -lc ").and_then(|(shell, rest)| {
        let login = shell.ends_with("sh") && !shell.contains(char::is_whitespace);
        (login && rest.len() >= 2).then_some(rest)
    });
    let Some(rest) = quoted else {
        return command.to_string();
    };
    let inner = &rest[1..rest.len() - 1];
    if rest.starts_with('\'') && rest.ends_with('\'') {
        return inner.replace(r#"'"'"'"#, "'").replace(r"'\''", "'");
    }
    if rest.starts_with('"') && rest.ends_with('"') {
        let mut out = String::new();
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            match (c, chars.peek()) {
                ('\\', Some(&next)) if matches!(next, '\\' | '"' | '$' | '`') => {
                    out.push(next);
                    chars.next();
                }
                _ => out.push(c),
            }
        }
        return out;
    }
    command.to_string()
}

/// Who sent a Buzz turn input: its event, the first line of a section.
fn sender(sections: &[crate::agent_activity::transcript_model::Section]) -> Option<String> {
    sections.iter().find_map(|section| {
        let first = section.body.split('\n').next().unwrap_or("");
        let event: Value = serde_json::from_str(first).ok()?;
        event.get("pubkey")?.as_str().map(str::to_string)
    })
}

fn str_of(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or("")
}

/// A value as text: strings as they are, anything else as JSON.
fn display(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn array(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// Codex item statuses as ACP tool statuses.
fn status(value: &str) -> &'static str {
    match value {
        "inProgress" => "in_progress",
        "failed" | "declined" => "failed",
        _ => "completed",
    }
}

/// The owner's request inside a Buzz turn input; other framing stays in
/// sections.
fn request(text: &str) -> String {
    let quoted = text
        .split('\n')
        .filter_map(|line| line.strip_prefix("Request: "))
        .rfind(|quoted| quoted.len() >= 2 && quoted.starts_with('"') && quoted.ends_with('"'));
    quoted
        .and_then(|quoted| serde_json::from_str::<String>(quoted).ok())
        .unwrap_or_else(|| text.to_string())
}

/// A user message's text parts, joined.
fn user_text(value: &Map<String, Value>) -> String {
    array(value.get("content"))
        .iter()
        .map(|part| str_of(part.get("text")))
        .collect::<Vec<_>>()
        .join("\n")
}

fn item(value: &Map<String, Value>, at: u64, steer: bool, name: Names) -> Option<Item> {
    let id = str_of(value.get("id")).to_string();
    let tool = |patch: ToolItem| ToolItem {
        tool_call_id: id.clone(),
        status: if patch.status.is_empty() {
            status(str_of(value.get("status"))).into()
        } else {
            patch.status
        },
        completed_at: value
            .get("durationMs")
            .and_then(Value::as_f64)
            .map(|duration| at + duration as u64),
        ..patch
    };
    let blank = ToolItem {
        tool_call_id: String::new(),
        title: String::new(),
        kind: "other".into(),
        status: String::new(),
        input: String::new(),
        output: String::new(),
        diffs: Vec::new(),
        paths: Vec::new(),
        completed_at: None,
        truncated: false,
        permission: None,
    };
    let kind = match str_of(value.get("type")) {
        "userMessage" => {
            let raw = user_text(value);
            let sections = prompt_sections(&raw);
            ItemKind::Prompt {
                text: request(&raw),
                author: sender(&sections).and_then(|pubkey| name(&pubkey)),
                sections,
                steer,
            }
        }
        // A turn's final answer is often empty: the reply went through send.
        "agentMessage" if str_of(value.get("text")).trim().is_empty() => return None,
        "agentMessage" => ItemKind::Message {
            text: str_of(value.get("text")).into(),
            message_id: None,
            posted: false,
        },
        "reasoning" => {
            let text = array(value.get("summary"))
                .iter()
                .map(|part| part.as_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n\n");
            if text.is_empty() {
                return None;
            }
            ItemKind::Thought {
                text,
                message_id: None,
            }
        }
        "plan" => ItemKind::Status(str_of(value.get("text")).into()),
        "commandExecution" => ItemKind::Tool(tool(ToolItem {
            title: shell_command(str_of(value.get("command"))),
            kind: "execute".into(),
            output: str_of(value.get("aggregatedOutput")).into(),
            ..blank
        })),
        "fileChange" => {
            let changes = array(value.get("changes"));
            ItemKind::Tool(tool(ToolItem {
                title: "Edit files".into(),
                kind: "edit".into(),
                paths: changes
                    .iter()
                    .map(|change| str_of(change.get("path")).to_string())
                    .collect(),
                output: changes
                    .iter()
                    .map(|change| str_of(change.get("diff")))
                    .collect::<Vec<_>>()
                    .join("\n"),
                ..blank
            }))
        }
        "mcpToolCall" => ItemKind::Tool(tool(ToolItem {
            title: format!(
                "{}.{}",
                str_of(value.get("server")),
                str_of(value.get("tool"))
            ),
            input: display(value.get("arguments")),
            output: display(
                value
                    .get("error")
                    .filter(|error| !error.is_null())
                    .or(value.get("result")),
            ),
            ..blank
        })),
        "dynamicToolCall"
            if str_of(value.get("namespace")) == "buzz"
                && str_of(value.get("tool")) == "send"
                && value.get("success") == Some(&Value::Bool(true))
                && !str_of(
                    value
                        .get("arguments")
                        .and_then(|arguments| arguments.get("text")),
                )
                .trim()
                .is_empty() =>
        {
            ItemKind::Message {
                text: str_of(
                    value
                        .get("arguments")
                        .and_then(|arguments| arguments.get("text")),
                )
                .into(),
                message_id: None,
                posted: true,
            }
        }
        "dynamicToolCall" => ItemKind::Tool(tool(ToolItem {
            title: [value.get("namespace"), value.get("tool")]
                .into_iter()
                .map(str_of)
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("."),
            input: display(value.get("arguments")),
            output: array(value.get("contentItems"))
                .iter()
                .map(|part| str_of(part.get("text")))
                .collect::<Vec<_>>()
                .join("\n"),
            status: if value.get("success") == Some(&Value::Bool(false)) {
                "failed".into()
            } else {
                String::new()
            },
            ..blank
        })),
        "webSearch" => ItemKind::Tool(tool(ToolItem {
            title: "Web search".into(),
            kind: "fetch".into(),
            input: str_of(value.get("query")).into(),
            ..blank
        })),
        _ => return None,
    };
    Some(Item { id, at, kind })
}

/// Codex turns, oldest first, as transcript turns for one conversation.
pub fn codex_turns(turns: &[Value], place: &Place, name: Names) -> Vec<TranscriptTurn> {
    turns
        .iter()
        .map(|turn| {
            let started_at = turn
                .get("startedAt")
                .and_then(Value::as_f64)
                .map_or(0, |at| (at * 1000.) as u64);
            let mut prompts = 0;
            let items = array(turn.get("items"))
                .iter()
                .filter_map(Value::as_object)
                .filter_map(|value| {
                    let prompt = str_of(value.get("type")) == "userMessage";
                    let steer = prompt && prompts > 0;
                    if prompt {
                        prompts += 1;
                    }
                    item(value, started_at, steer, name)
                })
                .collect();
            TranscriptTurn {
                turn_id: str_of(turn.get("id")).into(),
                channel_id: Some(place.channel_id.clone()),
                thread_root: Some(place.root.clone()),
                source: None,
                triggering_event_ids: Vec::new(),
                session_id: None,
                new_session: false,
                config: Vec::new(),
                started_at,
                ended_at: turn
                    .get("completedAt")
                    .and_then(Value::as_f64)
                    .filter(|at| *at != 0.)
                    .map(|at| (at * 1000.) as u64),
                error: Some(str_of(turn.pointer("/error/message")).to_string())
                    .filter(|error| !error.is_empty()),
                stop_reason: (str_of(turn.get("status")) == "interrupted")
                    .then(|| "interrupted".to_string()),
                context: None,
                partial: false,
                items,
            }
        })
        .collect()
}

/// A missing rollout (deleted, or never written) is not a read failure.
fn missing(error: &str) -> bool {
    let error = error.to_lowercase();
    ["not loaded", "no rollout", "not found"]
        .iter()
        .any(|text| error.contains(text))
}

/// A thread the conversation's name finds. `legacy` threads were named
/// without the tag, so their name alone does not tell same-named channels
/// apart.
struct Found {
    id: String,
    updated_at: f64,
    legacy: bool,
}

/// Whether `turns` were for the conversation, as their prompts' context
/// says.
fn started_for(turns: &[Value], lookup: &Lookup) -> bool {
    turns.iter().any(|turn| {
        array(turn.get("items")).iter().any(|value| {
            let Some(value) = value.as_object() else {
                return false;
            };
            if str_of(value.get("type")) != "userMessage" {
                return false;
            }
            let body = prompt_sections(&user_text(value))
                .into_iter()
                .find(|section| section.tag == "context")
                .map(|section| section.body)
                .unwrap_or_default();
            let Ok(Value::Object(context)) = serde_json::from_str::<Value>(&body) else {
                return false;
            };
            context.get("channel_id").and_then(Value::as_str) == Some(lookup.channel_id.as_str())
                && context.get("session_thread").and_then(Value::as_str) == lookup.root.as_deref()
        })
    })
}

/// The conversation's threads, newest first: named with its tag, or with
/// the readable name alone that earlier versions used.
async fn find_threads(rpc: &AppServer, lookup: &Lookup) -> Result<Vec<Found>, String> {
    let conversation = Conversation {
        channel_id: lookup.channel_id.clone(),
        name: lookup
            .name
            .clone()
            .unwrap_or_else(|| lookup.channel_id.clone()),
        root: lookup.root.clone(),
        top_level: false,
    };
    let tag = session_tag(
        &lookup.agent_pubkey,
        &lookup.channel_id,
        lookup.root.as_deref(),
    );
    let title = session_title(&lookup.agent_name, &conversation);
    let tagged = format!(" · {tag}");
    let mut found: Vec<Found> = Vec::new();
    for (term, legacy) in [(&tag, false), (&title, true)] {
        let page = rpc
            .request("thread/list", json!({ "searchTerm": term, "limit": 50 }))
            .await?;
        for thread in array(page.get("data")) {
            let id = str_of(thread.get("id"));
            let name = str_of(thread.get("name"));
            let named = if legacy {
                name == title
            } else {
                name.ends_with(&tagged)
            };
            if !name.is_empty() && named && !found.iter().any(|other| other.id == id) {
                found.push(Found {
                    id: id.into(),
                    updated_at: thread
                        .get("updatedAt")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.),
                    legacy,
                });
            }
        }
    }
    found.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at));
    Ok(found)
}

/// A thread's latest `limit` turns, newest first, and whether it has
/// earlier ones, or `None` when Codex no longer has it. One turn per
/// request: app-server's output frame bounds one full turn, not a page of
/// them.
async fn page(
    rpc: &AppServer,
    thread_id: &str,
    limit: usize,
) -> Result<Option<(Vec<Value>, bool)>, String> {
    let mut data = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut params = json!({
            "threadId": thread_id,
            "limit": 1,
            "sortDirection": "desc",
            "itemsView": "full",
        });
        if let Some(cursor) = &cursor {
            params["cursor"] = json!(cursor);
        }
        let next = match rpc.request("thread/turns/list", params).await {
            Ok(next) => next,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let turns = array(next.get("data"));
        data.extend(turns.iter().cloned());
        cursor = (!turns.is_empty())
            .then(|| {
                next.get("nextCursor")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();
        if cursor.is_none() || data.len() >= limit {
            break;
        }
    }
    data.truncate(limit);
    Ok(Some((data, cursor.is_some())))
}

/// Runs `using` with a short-lived app-server of its own, closed when done
/// or when `signal` aborts. Must run inside tokio.
async fn with_server<T>(
    spawn: &Spawner,
    signal: &Signal,
    using: impl AsyncFnOnce(&AppServer) -> Result<T, String>,
) -> Result<T, String> {
    let rpc = AppServer::new(None);
    let closer = {
        let (rpc, signal) = (rpc.clone(), signal.clone());
        tokio::spawn(async move {
            signal.wait().await;
            rpc.close().await;
        })
    };
    let result = async {
        signal.check()?;
        rpc.open(spawn, None).await?;
        signal.check()?;
        let result = using(&rpc).await?;
        signal.check()?;
        Ok(result)
    }
    .await;
    closer.abort();
    rpc.close().await;
    result
}

/// The conversation's newest thread and when it was last used, in
/// milliseconds, or `None` when Codex has no thread for it on the agent's
/// host.
pub async fn find_conversation(
    spawn: &Spawner,
    lookup: &Lookup,
    signal: &Signal,
) -> Result<Option<(String, u64)>, String> {
    with_server(spawn, signal, async |rpc| {
        for thread in find_threads(rpc, lookup).await? {
            let ours = !thread.legacy || {
                let turns = page(rpc, &thread.id, 1).await?.unwrap_or_default().0;
                started_for(&turns, lookup)
            };
            if ours {
                return Ok(Some((thread.id, (thread.updated_at * 1000.) as u64)));
            }
        }
        Ok(None)
    })
    .await
}

/// The latest turns of all the conversation's threads, oldest first, from
/// a short-lived app-server, or `None` when Codex has no history for it on
/// the agent's host. Must run inside tokio.
pub async fn read_transcript(
    spawn: &Spawner,
    lookup: &Lookup,
    signal: &Signal,
    name: Names<'_>,
) -> Result<Option<CodexRead>, String> {
    with_server(spawn, signal, async |rpc| {
        // The saved thread is the conversation's; one found by name may
        // need its context checked.
        let mut threads: Vec<(String, bool)> = lookup
            .thread_id
            .iter()
            .map(|id| (id.clone(), false))
            .collect();
        for thread in find_threads(rpc, lookup).await? {
            if !threads.iter().any(|(id, _)| *id == thread.id) {
                threads.push((thread.id, thread.legacy));
            }
        }
        let mut all: Vec<Value> = Vec::new();
        let (mut found, mut more, mut working) = (false, false, false);
        for (thread, legacy) in threads {
            let Some((turns, earlier)) = page(rpc, &thread, TURN_LIMIT).await? else {
                continue;
            };
            if legacy && !started_for(&turns, lookup) {
                continue;
            }
            found = true;
            more |= earlier;
            if lookup.thread_id.as_ref() == Some(&thread) {
                working = turns
                    .first()
                    .is_some_and(|turn| str_of(turn.get("status")) == "inProgress");
            }
            all.extend(turns.into_iter().rev());
        }
        if !found {
            return Ok(None);
        }
        let started = |turn: &Value| turn.get("startedAt").and_then(Value::as_f64).unwrap_or(0.);
        all.sort_by(|a, b| started(a).total_cmp(&started(b)));
        more |= all.len() > TURN_LIMIT;
        let latest = &all[all.len().saturating_sub(TURN_LIMIT)..];
        Ok(Some(CodexRead {
            transcript: Transcript {
                turns: codex_turns(latest, &lookup.place(), name),
                unknown_thread: 0,
            },
            more,
            working,
        }))
    })
    .await
}

#[cfg(test)]
mod tests;
