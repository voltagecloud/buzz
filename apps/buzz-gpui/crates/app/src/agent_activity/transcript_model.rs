//! Turns and items from observer telemetry (`activity-transcript.ts`): an
//! owner-only display projection that reads allowlisted ACP fields only.

use super::Record;
use super::turns::parse_time;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// Which turns to show (`TranscriptScope`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scope {
    pub agent: String,
    pub channel: Option<String>,
    /// Exact thread root; `None` is the whole channel, threads included.
    pub thread: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub tag: String,
    pub body: String,
}

/// ACP `diff` tool content; no old text is a new file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDiff {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permission {
    pub options: Vec<String>,
    pub outcome: Option<String>,
}

/// A tool call as the adapter sent it; only ACP's standard kind, status,
/// locations and content are read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolItem {
    pub tool_call_id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub input: String,
    pub output: String,
    pub diffs: Vec<ToolDiff>,
    pub paths: Vec<String>,
    pub completed_at: Option<u64>,
    pub truncated: bool,
    pub permission: Option<Permission>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    System(Vec<Section>),
    Prompt {
        text: String,
        author: Option<String>,
        sections: Vec<Section>,
        steer: bool,
    },
    Message {
        text: String,
        message_id: Option<String>,
        /// Posted to Buzz, rather than said only in the session.
        posted: bool,
    },
    Thought {
        text: String,
        message_id: Option<String>,
    },
    Tool(ToolItem),
    Plan(Vec<PlanEntry>),
    Status(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub at: u64,
    pub kind: ItemKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptTurn {
    pub turn_id: String,
    pub channel_id: Option<String>,
    /// `Some(None)` is the channel conversation; `None` is not
    /// recoverable.
    pub thread_root: Option<Option<String>>,
    pub source: Option<String>,
    pub triggering_event_ids: Vec<String>,
    pub session_id: Option<String>,
    pub new_session: bool,
    /// Selected ACP session config values, e.g. model and thinking level.
    pub config: Vec<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub error: Option<String>,
    pub stop_reason: Option<String>,
    pub context: Option<(f64, f64)>,
    /// No `turn_started` is loaded, so earlier items may be missing.
    pub partial: bool,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Transcript {
    pub turns: Vec<TranscriptTurn>,
    /// Turns hidden from a thread scope because their thread is unknown.
    pub unknown_thread: usize,
}

const ELIDED: &str = "…[elided ";
const PROMPT_METHODS: [&str; 3] = [
    "session/prompt",
    "_session/steering",
    "_goose/unstable/session/steer",
];

fn str_of(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or("")
}

fn event_id(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| super::hex64(s))
        .map(str::to_string)
}

/// Splits top-level `<tag attrs>` … `</tag>` framing; other text is kept
/// as a trailing "text" section (`promptSections`).
pub fn prompt_sections(text: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut loose: Vec<String> = Vec::new();
    let mut open: Option<(String, Vec<String>)> = None;
    for line in text.split('\n') {
        if let Some((tag, lines)) = &mut open {
            if line == format!("</{tag}>") {
                sections.push(Section {
                    tag: tag.clone(),
                    body: lines.join("\n"),
                });
                open = None;
            } else {
                lines.push(line.to_string());
            }
            continue;
        }
        match opening_tag(line) {
            Some(tag) => open = Some((tag, Vec::new())),
            None => loose.push(line.to_string()),
        }
    }
    // An unterminated section is ordinary text, not a dropped body.
    if let Some((tag, lines)) = open {
        loose.push(format!("<{tag}>"));
        loose.extend(lines);
    }
    let rest = loose.join("\n").trim().to_string();
    if !rest.is_empty() {
        sections.push(Section {
            tag: "text".into(),
            body: rest,
        });
    }
    sections
}

/// `^<([a-z][a-z0-9-]*)(?:\s[^<>]*)?>$`
fn opening_tag(line: &str) -> Option<String> {
    let inner = line.strip_prefix('<')?.strip_suffix('>')?;
    let end = inner
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        .unwrap_or(inner.len());
    let (tag, rest) = inner.split_at(end);
    if !tag.starts_with(|c: char| c.is_ascii_lowercase()) {
        return None;
    }
    if !rest.is_empty() && (!rest.starts_with(char::is_whitespace) || rest.contains(['<', '>'])) {
        return None;
    }
    Some(tag.to_string())
}

/// The triggering Buzz events' content and authors (`promptMessage`).
pub fn prompt_message(sections: &[Section]) -> (String, Option<String>) {
    let mut authors: Vec<String> = Vec::new();
    let mut contents = Vec::new();
    for section in sections
        .iter()
        .filter(|s| s.tag == "buzz-event" || s.tag == "buzz-events")
    {
        for block in split_events(&section.body) {
            let from = block
                .lines()
                .find_map(|line| line.strip_prefix("From: "))
                .map(unescaped);
            let Some(start) = block.find("\nContent: ") else {
                continue;
            };
            let end = block.rfind("\nTags: ").filter(|end| *end > start);
            let content = &block[start + 10..end.unwrap_or(block.len())];
            contents.push(unescaped(content.trim()));
            if let Some(from) = from {
                let name = strip_key_suffix(&from);
                if !authors.contains(&name) {
                    authors.push(name);
                }
            }
        }
    }
    let author = (!authors.is_empty()).then(|| authors.join(", "));
    (contents.join("\n\n"), author)
}

/// Undoes the prompt's escaping of `<` and `>` in untrusted text.
fn unescaped(text: &str) -> String {
    text.replace("\\u003c", "<").replace("\\u003e", ">")
}

/// Splits before each `\n--- Event N ` line.
fn split_events(body: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut start = 0;
    let mut search = 0;
    while let Some(found) = body[search..].find("\n--- Event ") {
        let at = search + found;
        let rest = &body[at + 11..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with(' ') {
            blocks.push(&body[start..at]);
            start = at + 1;
        }
        search = at + 1;
    }
    blocks.push(&body[start..]);
    blocks
}

/// Drops a trailing ` (npub…)` or ` (hex…)`.
fn strip_key_suffix(from: &str) -> String {
    if from.ends_with(')')
        && let Some(open) = from.rfind(" (")
    {
        let inner = &from[open + 2..];
        if inner.starts_with("npub") || inner.starts_with("hex") {
            return from[..open].to_string();
        }
    }
    from.to_string()
}

/// Thread evidence from the prompt's `<context>`: `Some(Some(root))`, the
/// channel (`Some(None)`), or unknown.
fn prompt_thread(sections: &[Section]) -> Option<Option<String>> {
    let context = &sections.iter().find(|s| s.tag == "context")?.body;
    for line in context.lines() {
        if let Some(root) = line.strip_prefix("Thread root: ")
            && !root.contains(char::is_whitespace)
            && super::hex64(root)
        {
            return Some(Some(root.to_string()));
        }
    }
    context
        .lines()
        .any(|line| line == "Scope: channel" || line == "Scope: dm")
        .then_some(None)
}

fn display(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(value) => serde_json::to_string_pretty(value).unwrap_or_default(),
    }
}

/// A block that is one fenced code block shows its body.
fn unfence(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("```")
        && let Some(newline) = rest.find('\n')
    {
        let body = &rest[newline + 1..];
        let trimmed = body.trim_end();
        if let Some(inner) = trimmed.strip_suffix("```") {
            return inner.strip_suffix('\n').unwrap_or(inner).to_string();
        }
    }
    text.to_string()
}

fn tool_content(update: &Map<String, Value>) -> (Vec<ToolDiff>, String) {
    let mut diffs = Vec::new();
    let mut text = Vec::new();
    for entry in update
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let block = entry.get("content");
        if entry.get("type").and_then(Value::as_str) == Some("diff")
            && let Some(new_text) = entry.get("newText").and_then(Value::as_str)
        {
            diffs.push(ToolDiff {
                path: str_of(entry.get("path")).to_string(),
                old_text: entry
                    .get("oldText")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                new_text: new_text.to_string(),
            });
        } else if block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("text") {
            let value = str_of(block.and_then(|b| b.get("text")));
            if !value.is_empty() {
                text.push(unfence(value));
            }
        }
    }
    let output = if text.is_empty() {
        display(update.get("rawOutput"))
    } else {
        text.join("\n")
    };
    (diffs, output)
}

/// Selected values of ACP config options, named (`configLabels`).
fn config_labels(options: Option<&Value>) -> Option<Vec<String>> {
    let options = options?.as_array()?;
    Some(
        options
            .iter()
            .filter_map(|option| {
                let current = option
                    .get("currentValue")?
                    .as_str()
                    .filter(|s| !s.is_empty())?;
                let choice = option
                    .get("options")
                    .and_then(Value::as_array)
                    .and_then(|choices| {
                        choices
                            .iter()
                            .find(|c| c.get("value").and_then(Value::as_str) == Some(current))
                    });
                let named = str_of(choice.and_then(|c| c.get("name")));
                let label = if named.is_empty() { current } else { named };
                let name = str_of(option.get("name"));
                Some(if !name.is_empty() && !label.starts_with(name) {
                    format!("{name}: {label}")
                } else {
                    label.to_string()
                })
            })
            .collect(),
    )
}

struct Draft {
    turn: TranscriptTurn,
    tools: HashMap<String, usize>,
    plan: Option<usize>,
    pi_startup: Option<String>,
    payload_thread: Option<String>,
    prompt_thread: Option<Option<String>>,
}

fn tool_index(draft: &mut Draft, call: &str, at: u64, key: &str) -> usize {
    if let Some(index) = draft.tools.get(call) {
        return *index;
    }
    draft.turn.items.push(Item {
        id: key.to_string(),
        at,
        kind: ItemKind::Tool(ToolItem {
            tool_call_id: call.to_string(),
            title: String::new(),
            kind: String::new(),
            status: "pending".into(),
            input: String::new(),
            output: String::new(),
            diffs: Vec::new(),
            paths: Vec::new(),
            completed_at: None,
            truncated: false,
            permission: None,
        }),
    });
    let index = draft.turn.items.len() - 1;
    draft.tools.insert(call.to_string(), index);
    index
}

fn tool_mut(draft: &mut Draft, index: usize) -> &mut ToolItem {
    match &mut draft.turn.items[index].kind {
        ItemKind::Tool(tool) => tool,
        _ => unreachable!("tool index points at a tool"),
    }
}

/// Joins message and thought chunks: an explicit message id joins across
/// tools, unkeyed chunks only join the adjacent one.
fn chunk(draft: &mut Draft, thought: bool, text: &str, message: &str, id: &str, at: u64) {
    // The item's message id when it is a chunk of this type.
    let same = |item: &Item| match (&item.kind, thought) {
        (ItemKind::Thought { message_id, .. }, true)
        | (ItemKind::Message { message_id, .. }, false) => Some(message_id.clone()),
        _ => None,
    };
    let items = &mut draft.turn.items;
    let target = if !message.is_empty() {
        items
            .iter()
            .position(|item| same(item).flatten().as_deref() == Some(message))
    } else {
        items
            .len()
            .checked_sub(1)
            .filter(|last| same(&items[*last]) == Some(None))
    };
    if let Some(index) = target {
        if let ItemKind::Thought { text: joined, .. } | ItemKind::Message { text: joined, .. } =
            &mut items[index].kind
        {
            joined.push_str(text);
        }
        return;
    }
    let message_id = (!message.is_empty()).then(|| message.to_string());
    let text = text.to_string();
    items.push(Item {
        id: id.to_string(),
        at,
        kind: if thought {
            ItemKind::Thought { text, message_id }
        } else {
            ItemKind::Message {
                text,
                message_id,
                posted: false,
            }
        },
    });
}

/// The transcript of `scope` from `records` (`activityTranscript`).
pub fn transcript(records: &[Arc<Record>], scope: &Scope) -> Transcript {
    let mut events: Vec<(String, u64, usize, Value)> = Vec::new();
    for record in records.iter().filter(|r| r.agent == scope.agent) {
        let Ok(raw) = serde_json::from_str::<Value>(&record.plaintext) else {
            continue;
        };
        let children: Vec<Value> = if raw.get("kind").and_then(Value::as_str) == Some("batch") {
            match raw
                .get("payload")
                .and_then(|p| p.get("events"))
                .and_then(Value::as_array)
            {
                Some(events) => events.clone(),
                None => continue,
            }
        } else {
            vec![raw]
        };
        for (index, event) in children.into_iter().enumerate() {
            if !event.is_object() {
                continue;
            }
            // Unscoped children never inherit a channel.
            if let Some(channel) = &scope.channel
                && event.get("channelId").and_then(Value::as_str) != Some(channel)
            {
                continue;
            }
            let at = parse_time(str_of(event.get("timestamp")))
                .filter(|at| *at <= record.received_at + 5000)
                .unwrap_or(record.received_at);
            let order = events.len();
            events.push((format!("{}:{index}", record.id), at, order, event));
        }
    }
    events.sort_by_key(|(_, at, order, _)| (*at, *order));

    let mut drafts: Vec<Draft> = Vec::new();
    let mut by_turn: HashMap<String, usize> = HashMap::new();
    // JSON-RPC ids are per agent process and direction: key by pool slot.
    let mut prompts: HashMap<String, usize> = HashMap::new();
    let mut permissions: HashMap<String, (usize, usize, Vec<Value>)> = HashMap::new();

    for (id, at, _, event) in events {
        let turn_id = str_of(event.get("turnId"));
        if turn_id.is_empty() {
            continue;
        }
        let kind = str_of(event.get("kind")).to_string();
        let index = *by_turn.entry(turn_id.to_string()).or_insert_with(|| {
            drafts.push(Draft {
                turn: TranscriptTurn {
                    turn_id: turn_id.to_string(),
                    channel_id: event
                        .get("channelId")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    thread_root: None,
                    source: None,
                    triggering_event_ids: Vec::new(),
                    session_id: None,
                    new_session: false,
                    config: Vec::new(),
                    started_at: at,
                    ended_at: None,
                    error: None,
                    stop_reason: None,
                    context: None,
                    partial: true,
                    items: Vec::new(),
                },
                tools: HashMap::new(),
                plan: None,
                pi_startup: None,
                payload_thread: None,
                prompt_thread: None,
            });
            drafts.len() - 1
        });
        let draft = &mut drafts[index];
        let session = str_of(event.get("sessionId"));
        if !session.is_empty() {
            draft.turn.session_id = Some(session.to_string());
        }
        let empty = Map::new();
        let payload = event
            .get("payload")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        if let Some(root) = event_id(payload.get("threadRootEventId")) {
            draft.payload_thread = Some(root);
        }
        let slot = match event.get("agentIndex") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(value) => value.to_string(),
        };
        let rpc_key = |id: &Value| format!("{slot}:{id}");
        match kind.as_str() {
            "turn_started" => {
                draft.turn.partial = false;
                draft.turn.started_at = parse_time(str_of(event.get("startedAt"))).unwrap_or(at);
                let source = str_of(payload.get("source"));
                if !source.is_empty() {
                    draft.turn.source = Some(source.to_string());
                }
                draft.turn.triggering_event_ids = payload
                    .get("triggeringEventIds")
                    .and_then(Value::as_array)
                    .map(|ids| ids.iter().filter_map(|id| event_id(Some(id))).collect())
                    .unwrap_or_default();
            }
            "session_resolved" => {
                let session = str_of(payload.get("sessionId"));
                if !session.is_empty() {
                    draft.turn.session_id = Some(session.to_string());
                }
                draft.turn.new_session = payload.get("isNewSession") == Some(&Value::Bool(true));
            }
            "session_config_captured" => {
                if let Some(labels) = config_labels(payload.get("configOptions")) {
                    draft.turn.config = labels;
                }
            }
            "turn_completed" => {
                draft.turn.ended_at.get_or_insert(at);
            }
            "turn_error" | "agent_panic" => {
                draft.turn.ended_at.get_or_insert(at);
                let error: String = str_of(payload.get("error")).chars().take(2000).collect();
                let outcome = str_of(payload.get("outcome"));
                draft.turn.error = Some(if !error.is_empty() {
                    error
                } else if !outcome.is_empty() {
                    outcome.to_string()
                } else {
                    kind.clone()
                });
            }
            "acp_write" => {
                let method = str_of(payload.get("method"));
                let params = payload
                    .get("params")
                    .and_then(Value::as_object)
                    .unwrap_or(&empty);
                if method == "session/new" {
                    let meta = params.get("_meta").and_then(|m| m.get("systemPrompt"));
                    let system = [
                        str_of(params.get("systemPrompt")),
                        str_of(meta),
                        str_of(meta.and_then(|m| m.get("replace"))),
                        str_of(meta.and_then(|m| m.get("append"))),
                    ]
                    .into_iter()
                    .find(|s| !s.is_empty());
                    if let Some(system) = system {
                        draft.turn.items.push(Item {
                            id: id.clone(),
                            at,
                            kind: ItemKind::System(prompt_sections(system)),
                        });
                    }
                } else if PROMPT_METHODS.contains(&method)
                    && let Some(prompt) = params.get("prompt").and_then(Value::as_array)
                {
                    let text = prompt
                        .iter()
                        .map(|block| str_of(block.get("text")))
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let sections = prompt_sections(&text);
                    let (message, author) = prompt_message(&sections);
                    if let Some(thread) = prompt_thread(&sections) {
                        draft.prompt_thread = Some(thread);
                    }
                    // Non-Buzz prompts keep their unframed text.
                    let shown = if !message.is_empty() {
                        message
                    } else {
                        sections
                            .iter()
                            .find(|s| s.tag == "text")
                            .map(|s| s.body.clone())
                            .unwrap_or(text)
                    };
                    draft.turn.items.push(Item {
                        id: id.clone(),
                        at,
                        kind: ItemKind::Prompt {
                            text: shown,
                            author,
                            sections,
                            steer: method != "session/prompt",
                        },
                    });
                    if method == "session/prompt"
                        && let Some(rpc) = payload.get("id")
                    {
                        prompts.insert(rpc_key(rpc), index);
                    }
                } else if method.is_empty()
                    && let Some(rpc) = payload.get("id")
                {
                    // Our reply to an agent request, e.g. a permission.
                    let outcome = payload
                        .get("result")
                        .and_then(|r| r.get("outcome"))
                        .and_then(Value::as_object);
                    if let (Some((turn, item, options)), Some(outcome)) =
                        (permissions.get(&rpc_key(rpc)), outcome)
                    {
                        let option = options
                            .iter()
                            .find(|o| o.get("optionId") == outcome.get("optionId"));
                        let label = if str_of(outcome.get("outcome")) == "cancelled" {
                            "Cancelled".to_string()
                        } else {
                            [
                                str_of(option.and_then(|o| o.get("name"))),
                                str_of(option.and_then(|o| o.get("kind"))),
                                str_of(outcome.get("optionId")),
                            ]
                            .into_iter()
                            .find(|s| !s.is_empty())
                            .unwrap_or("")
                            .to_string()
                        };
                        let (turn, item) = (*turn, *item);
                        if let ItemKind::Tool(tool) = &mut drafts[turn].turn.items[item].kind
                            && let Some(permission) = &mut tool.permission
                        {
                            permission.outcome = Some(label);
                        }
                    }
                }
            }
            "acp_read" => {
                let method = str_of(payload.get("method"));
                let params = payload
                    .get("params")
                    .and_then(Value::as_object)
                    .unwrap_or(&empty);
                if method.is_empty()
                    && let Some(rpc) = payload.get("id")
                {
                    let result = payload.get("result");
                    let stop = str_of(result.and_then(|r| r.get("stopReason")));
                    if let Some(prompt) = prompts.get(&rpc_key(rpc))
                        && !stop.is_empty()
                    {
                        drafts[*prompt].turn.stop_reason = Some(stop.to_string());
                    }
                    // pi-acp repeats its startup banner as an agent message;
                    // it is not a reply. Raw keeps it.
                    let banner = str_of(
                        result
                            .and_then(|r| r.get("_meta"))
                            .and_then(|m| m.get("piAcp"))
                            .and_then(|p| p.get("startupInfo")),
                    );
                    if !banner.is_empty() {
                        drafts[index].pi_startup = Some(banner.to_string());
                    }
                } else if method == "session/request_permission" {
                    let call = params.get("toolCall");
                    let call_id = str_of(call.and_then(|c| c.get("toolCallId")));
                    let call_id = if call_id.is_empty() {
                        format!("permission:{id}")
                    } else {
                        call_id.to_string()
                    };
                    let item = tool_index(draft, &call_id, at, &id);
                    let options: Vec<Value> = params
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|o| o.iter().filter(|v| v.is_object()).cloned().collect())
                        .unwrap_or_default();
                    let tool = tool_mut(draft, item);
                    if tool.title.is_empty() {
                        tool.title = str_of(call.and_then(|c| c.get("title"))).to_string();
                    }
                    tool.permission = Some(Permission {
                        options: options
                            .iter()
                            .map(|o| {
                                let name = str_of(o.get("name"));
                                if name.is_empty() {
                                    str_of(o.get("kind")).to_string()
                                } else {
                                    name.to_string()
                                }
                            })
                            .collect(),
                        outcome: None,
                    });
                    if let Some(rpc) = payload.get("id") {
                        permissions.insert(rpc_key(rpc), (index, item, options));
                    }
                } else if method == "session/update" {
                    let update = params
                        .get("update")
                        .and_then(Value::as_object)
                        .unwrap_or(&empty);
                    read_update(draft, update, &id, at);
                }
            }
            _ => {}
        }
        if !str_of(payload.get("elided")).is_empty() {
            drafts[index].turn.items.push(Item {
                id: id.clone(),
                at,
                kind: ItemKind::Status(format!(
                    "A {kind} frame was too large and was elided by the agent."
                )),
            });
        }
    }

    // A reused session shows its last observed config.
    drafts.sort_by_key(|draft| draft.turn.started_at);
    let mut configs: HashMap<String, Vec<String>> = HashMap::new();
    for draft in &mut drafts {
        let Some(session) = draft.turn.session_id.clone() else {
            continue;
        };
        if !draft.turn.config.is_empty() {
            configs.insert(session, draft.turn.config.clone());
        } else {
            draft.turn.config = configs.get(&session).cloned().unwrap_or_default();
        }
    }

    let mut unknown_thread = 0;
    let mut turns = Vec::new();
    for draft in drafts {
        let mut turn = draft.turn;
        // Older harnesses omit threadRootEventId; the prompt names it.
        turn.thread_root = match draft.payload_thread {
            Some(root) => Some(Some(root)),
            None => draft.prompt_thread,
        };
        if let Some(thread) = &scope.thread
            && turn.thread_root.as_ref().and_then(Option::as_ref) != Some(thread)
        {
            if turn.thread_root.is_none() {
                unknown_thread += 1;
            }
            continue;
        }
        turns.push(turn);
    }
    Transcript {
        turns,
        unknown_thread,
    }
}

fn read_update(draft: &mut Draft, update: &Map<String, Value>, id: &str, at: u64) {
    let kind = str_of(update.get("sessionUpdate"));
    match kind {
        "agent_message_chunk" | "agent_thought_chunk" => {
            let content = update.get("content");
            let text = str_of(content.and_then(|c| c.get("text")));
            if kind == "agent_message_chunk" && draft.pi_startup.as_deref() == Some(text) {
                draft.pi_startup = None;
                return;
            }
            let block = content
                .and_then(|c| c.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("text");
            if !text.is_empty() && block == "text" {
                chunk(
                    draft,
                    kind == "agent_thought_chunk",
                    text,
                    str_of(update.get("messageId")),
                    id,
                    at,
                );
            }
        }
        "tool_call" | "tool_call_update" => {
            let call = str_of(update.get("toolCallId"));
            if call.is_empty() {
                return;
            }
            let index = tool_index(draft, call, at, id);
            let item_at = draft.turn.items[index].at;
            let tool = tool_mut(draft, index);
            for (field, key) in [
                (&mut tool.title, "title"),
                (&mut tool.kind, "kind"),
                (&mut tool.status, "status"),
            ] {
                let value = str_of(update.get(key));
                if !value.is_empty() {
                    *field = value.to_string();
                }
            }
            if let Some(input) = update.get("rawInput") {
                tool.input = display(Some(input));
            }
            let (diffs, output) = tool_content(update);
            if !output.is_empty() {
                tool.output = output;
            }
            if !diffs.is_empty() {
                tool.diffs = diffs;
            }
            let located: Vec<String> = update
                .get("locations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|l| str_of(l.get("path")).to_string())
                .filter(|p| !p.is_empty())
                .collect();
            if !located.is_empty() {
                tool.paths = located;
            }
            if matches!(tool.status.as_str(), "completed" | "failed") && tool.completed_at.is_none()
            {
                tool.completed_at = Some(at.max(item_at));
            }
            let elided = [&tool.input, &tool.output]
                .into_iter()
                .any(|t| t.contains(ELIDED))
                || tool.diffs.iter().any(|d| {
                    d.new_text.contains(ELIDED)
                        || d.old_text.as_ref().is_some_and(|t| t.contains(ELIDED))
                });
            tool.truncated |= elided;
        }
        "plan" => {
            let Some(entries) = update.get("entries").and_then(Value::as_array) else {
                return;
            };
            let entries: Vec<PlanEntry> = entries
                .iter()
                .filter_map(|entry| {
                    let content = str_of(entry.get("content"));
                    (!content.is_empty()).then(|| PlanEntry {
                        content: content.to_string(),
                        status: str_of(entry.get("status")).to_string(),
                    })
                })
                .collect();
            match draft.plan {
                Some(index) => draft.turn.items[index].kind = ItemKind::Plan(entries),
                None => {
                    draft.turn.items.push(Item {
                        id: id.to_string(),
                        at,
                        kind: ItemKind::Plan(entries),
                    });
                    draft.plan = Some(draft.turn.items.len() - 1);
                }
            }
        }
        "current_mode_update" => {
            let mode = str_of(update.get("currentModeId"));
            if !mode.is_empty() {
                draft.turn.items.push(Item {
                    id: id.to_string(),
                    at,
                    kind: ItemKind::Status(format!("Mode: {mode}")),
                });
            }
        }
        "config_option_update" => {
            if let Some(labels) = config_labels(update.get("configOptions")) {
                draft.turn.config = labels;
            }
        }
        "usage_update" => {
            let used = update.get("used").and_then(Value::as_f64);
            let size = update.get("size").and_then(Value::as_f64);
            if let (Some(used), Some(size)) = (used, size) {
                draft.turn.context = Some((used, size));
            }
        }
        _ => {}
    }
}

/// `12.3s`, `45s`, `2m05s` (`duration`).
pub fn duration(ms: u64) -> String {
    if ms < 60_000 {
        let seconds = ms as f64 / 1000.;
        return if ms < 10_000 {
            format!("{seconds:.1}s")
        } else {
            format!("{seconds:.0}s")
        };
    }
    format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(id: &str, at: u64, plaintext: Value) -> Arc<Record> {
        Arc::new(Record {
            id: id.into(),
            agent: "a".repeat(64),
            created_at: at / 1000,
            plaintext: plaintext.to_string().into(),
            received_at: at,
            historical: false,
            kind: "batch".into(),
            channel_ids: Vec::new(),
        })
    }

    fn batch(events: Vec<Value>) -> Value {
        json!({"kind": "batch", "payload": {"events": events}})
    }

    fn scope() -> Scope {
        Scope {
            agent: "a".repeat(64),
            ..Default::default()
        }
    }

    const T0: &str = "2026-10-10T12:00:00Z";
    const T1: &str = "2026-10-10T12:00:01Z";
    const T2: &str = "2026-10-10T12:00:02Z";
    const NOW: u64 = 1_791_633_610_000;

    fn event(kind: &str, at: &str, payload: Value) -> Value {
        json!({"kind": kind, "turnId": "t1", "channelId": "c1", "timestamp": at, "payload": payload})
    }

    fn update(at: &str, update: Value) -> Value {
        event(
            "acp_read",
            at,
            json!({"method": "session/update", "params": {"update": update}}),
        )
    }

    #[test]
    fn sections_split_framing_and_keep_loose_text() {
        let sections = prompt_sections("<context>\nScope: channel\n</context>\nhello\n<open>\nx");
        assert_eq!(sections[0].tag, "context");
        assert_eq!(sections[0].body, "Scope: channel");
        assert_eq!(sections[1].tag, "text");
        assert_eq!(sections[1].body, "hello\n<open>\nx");
        assert!(opening_tag("<buzz-event id=\"1\">").is_some());
        assert!(opening_tag("<Tag>").is_none());
        assert!(opening_tag("<a<b>").is_none());
    }

    #[test]
    fn prompts_show_the_buzz_message_and_its_thread() {
        let root = "b".repeat(64);
        let text = format!(
            "<context>\nThread root: {root}\n</context>\n<buzz-event>\n--- Event 1 ---\nFrom: Alice (npub1xyz)\nContent: Please fix it\nTags: []\n</buzz-event>"
        );
        let records = vec![record(
            "r1",
            NOW,
            batch(vec![
                event("turn_started", T0, json!({"source": "mention"})),
                event(
                    "acp_write",
                    T1,
                    json!({"method": "session/prompt", "id": 4, "params": {"prompt": [{"type": "text", "text": text}]}}),
                ),
                event(
                    "acp_read",
                    T2,
                    json!({"id": 4, "result": {"stopReason": "end_turn"}}),
                ),
                event("turn_completed", T2, json!({})),
            ]),
        )];
        let transcript = transcript(&records, &scope());
        let turn = &transcript.turns[0];
        assert!(!turn.partial);
        assert_eq!(turn.thread_root, Some(Some(root.clone())));
        assert_eq!(turn.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(turn.ended_at, Some(parse_time(T2).unwrap()));
        let ItemKind::Prompt {
            text,
            author,
            steer,
            sections,
        } = &turn.items[0].kind
        else {
            panic!("a prompt");
        };
        assert_eq!(text, "Please fix it");
        assert_eq!(author.as_deref(), Some("Alice"));
        assert!(!steer);
        assert_eq!(sections.len(), 2);

        let other = Scope {
            thread: Some("c".repeat(64)),
            ..scope()
        };
        assert!(super::transcript(&records, &other).turns.is_empty());
    }

    #[test]
    fn chunks_merge_by_message_id_or_adjacency() {
        let records = vec![record(
            "r1",
            NOW,
            batch(vec![
                update(
                    T0,
                    json!({"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": {"type": "text", "text": "Hel"}}),
                ),
                update(
                    T0,
                    json!({"sessionUpdate": "tool_call", "toolCallId": "x", "title": "Read", "kind": "read", "status": "in_progress"}),
                ),
                update(
                    T0,
                    json!({"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": {"type": "text", "text": "lo"}}),
                ),
                update(
                    T1,
                    json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "a"}}),
                ),
                update(
                    T1,
                    json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "b"}}),
                ),
                update(
                    T2,
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "x"}}),
                ),
                update(
                    T2,
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "text": "y"}}),
                ),
            ]),
        )];
        let turn = &transcript(&records, &scope()).turns[0];
        assert!(turn.partial, "no turn_started loaded");
        let kinds: Vec<&ItemKind> = turn.items.iter().map(|i| &i.kind).collect();
        assert!(matches!(kinds[0], ItemKind::Message { text, .. } if text == "Hello"));
        assert!(matches!(kinds[1], ItemKind::Tool(_)));
        assert!(matches!(kinds[2], ItemKind::Thought { text, .. } if text == "ab"));
        assert!(matches!(kinds[3], ItemKind::Message { text, .. } if text == "x"));
        assert_eq!(kinds.len(), 4);
    }

    #[test]
    fn tools_collect_content_permission_and_elision() {
        let records = vec![record(
            "r1",
            NOW,
            batch(vec![
                event(
                    "acp_read",
                    T0,
                    json!({"method": "session/request_permission", "id": 9, "params": {"toolCall": {"toolCallId": "e1", "title": "Edit main.rs"}, "options": [{"optionId": "ok", "name": "Allow once"}]}}),
                ),
                event(
                    "acp_write",
                    T0,
                    json!({"id": 9, "result": {"outcome": {"outcome": "selected", "optionId": "ok"}}}),
                ),
                update(
                    T1,
                    json!({"sessionUpdate": "tool_call_update", "toolCallId": "e1", "kind": "edit", "status": "completed", "rawInput": {"path": "main.rs"}, "locations": [{"path": "/src/main.rs"}], "content": [
                        {"type": "diff", "path": "main.rs", "oldText": "a\nb\n", "newText": "a\nc\n"},
                        {"type": "content", "content": {"type": "text", "text": "```\nok…[elided 10 bytes]\n```"}}
                    ]}),
                ),
            ]),
        )];
        let turn = &transcript(&records, &scope()).turns[0];
        let ItemKind::Tool(tool) = &turn.items[0].kind else {
            panic!("a tool");
        };
        assert_eq!(tool.title, "Edit main.rs");
        assert_eq!(
            (tool.kind.as_str(), tool.status.as_str()),
            ("edit", "completed")
        );
        assert_eq!(
            tool.permission.as_ref().unwrap().outcome.as_deref(),
            Some("Allow once")
        );
        assert_eq!(tool.output, "ok…[elided 10 bytes]");
        assert!(tool.truncated);
        assert_eq!(tool.paths, vec!["/src/main.rs"]);
        assert_eq!(tool.diffs[0].old_text.as_deref(), Some("a\nb\n"));
        assert_eq!(tool.completed_at, parse_time(T1));
        assert!(tool.input.contains("\"path\": \"main.rs\""));
    }

    #[test]
    fn plans_status_errors_and_config() {
        let records = vec![record(
            "r1",
            NOW,
            batch(vec![
                json!({"kind": "turn_started", "turnId": "t1", "channelId": "c1", "sessionId": "s", "timestamp": T0, "payload": {}}),
                json!({"kind": "session_config_captured", "turnId": "t1", "channelId": "c1", "timestamp": T0, "payload": {"configOptions": [
                    {"name": "Model", "currentValue": "o", "options": [{"value": "o", "name": "Opus"}]},
                    {"name": "Thinking", "currentValue": "Thinking: high"}
                ]}}),
                update(
                    T0,
                    json!({"sessionUpdate": "plan", "entries": [{"content": "a", "status": "pending"}]}),
                ),
                update(
                    T1,
                    json!({"sessionUpdate": "plan", "entries": [{"content": "a", "status": "completed"}, {"content": "b", "status": "pending"}]}),
                ),
                update(
                    T1,
                    json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
                ),
                update(
                    T1,
                    json!({"sessionUpdate": "usage_update", "used": 12000, "size": 200000}),
                ),
                event("turn_error", T2, json!({"error": "boom", "elided": "yes"})),
                json!({"kind": "turn_liveness", "turnId": "t2", "channelId": "c1", "sessionId": "s", "timestamp": T2, "payload": {}}),
            ]),
        )];
        let transcript = transcript(&records, &scope());
        let turn = &transcript.turns[0];
        assert_eq!(turn.config, vec!["Model: Opus", "Thinking: high"]);
        assert_eq!(turn.error.as_deref(), Some("boom"));
        assert_eq!(turn.context, Some((12000., 200000.)));
        assert!(
            matches!(&turn.items[0].kind, ItemKind::Plan(entries) if entries.len() == 2 && entries[0].status == "completed")
        );
        assert!(matches!(&turn.items[1].kind, ItemKind::Status(text) if text == "Mode: plan"));
        assert!(
            matches!(&turn.items[2].kind, ItemKind::Status(text) if text == "A turn_error frame was too large and was elided by the agent.")
        );
        // The later turn in the same session shows its config.
        assert_eq!(transcript.turns[1].config, turn.config);
    }

    #[test]
    fn system_prompt_and_unknown_threads() {
        let records = vec![record(
            "r1",
            NOW,
            batch(vec![event(
                "acp_write",
                T0,
                json!({"method": "session/new", "params": {"_meta": {"systemPrompt": {"append": "<rules>\nbe kind\n</rules>"}}}}),
            )]),
        )];
        let all = transcript(&records, &scope());
        assert!(matches!(&all.turns[0].items[0].kind, ItemKind::System(s) if s[0].tag == "rules"));
        let thread = Scope {
            thread: Some("d".repeat(64)),
            ..scope()
        };
        assert_eq!(transcript(&records, &thread).unknown_thread, 1);
    }

    #[test]
    fn durations_read_like_the_reference() {
        assert_eq!(duration(1234), "1.2s");
        assert_eq!(duration(45_000), "45s");
        assert_eq!(duration(125_000), "2m05s");
        assert_eq!(unfence("```rust\nfn x() {}\n```"), "fn x() {}");
        assert_eq!(unfence("plain"), "plain");
    }
}
