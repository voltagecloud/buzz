//! The Buzz tools: what an agent does in Buzz, in the wire formats the
//! `buzz` CLI writes (buzz-app's `src/buzz-mcp/tools.ts`). Names, arguments
//! and descriptions are kept short because every session pays for them in
//! context. Each tool answers in plain text.

use super::{BuzzClient, BuzzEvent, Context, Template, Upload};
use buzz_relay_client::lifecycle::{self, Action};
use nostr::nips::nip19::FromBech32;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

pub type Args = Map<String, Value>;
type ToolResult = Result<String, String>;
/// Whether a roster shows a member change to a key.
type Shown<'a> = dyn Fn(&[(String, String)], &str) -> bool + 'a;

/// A tool's answer: text, or an image (base64 `data`) for the agent to see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Text(String),
    Image { data: String, mime: String },
}

#[cfg(test)]
impl Reply {
    /// The text, or a short description of an image.
    pub fn text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Image { mime, .. } => format!("[{mime}]"),
        }
    }
}

pub(super) fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    json!({ "name": name, "description": description, "inputSchema": schema })
}

/// The tools as `tools/list` describes them: Buzz's, then those plugins
/// added ([`super::added`]).
pub fn tools() -> Vec<Value> {
    let mut tools = buzz_tools();
    tools.extend(super::added::listed());
    tools
}

/// Whether `name` is one of Buzz's own tools.
pub fn buzz_tool(name: &str) -> bool {
    name == "media_get"
        || buzz_tools()
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
}

fn buzz_tools() -> Vec<Value> {
    let str = json!({ "type": "string" });
    let strs = json!({ "type": "array", "items": { "type": "string" } });
    let int = json!({ "type": "integer" });
    vec![
        tool(
            "send",
            "Post a message. Without channel, replies in the current thread. A reply goes in its message's thread.",
            json!({
                "text": str, "path": str, "channel": str, "reply": str,
                "files": strs, "mentions": strs,
                "audience": {
                    "type": "string",
                    "enum": ["everyone", "agents"],
                    "description": "agents: only other agents need this (delegating, asking or answering an agent)",
                },
                "final": {
                    "type": "boolean",
                    "description": "Your last message this turn; ends the turn.",
                },
            }),
            &["audience"],
        ),
        tool(
            "edit",
            "Edit your message.",
            json!({ "id": str, "text": str }),
            &["id", "text"],
        ),
        tool(
            "react",
            "React to a message.",
            json!({ "id": str, "emoji": str, "off": { "type": "boolean" } }),
            &["id", "emoji"],
        ),
        tool(
            "read",
            "Read a thread (default: current), a channel's latest, or search with q.",
            json!({ "channel": str, "thread": str, "q": str, "limit": int, "before": int }),
            &[],
        ),
        tool(
            "channels",
            "Channels you are in; with all, every one you can see.",
            json!({ "q": str, "all": { "type": "boolean" } }),
            &[],
        ),
        tool(
            "channel",
            "Channel details (default: current), or an action on it. create may use your owner's template, by name. delete is permanent.",
            json!({
                "channel": str,
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "join", "leave", "archive", "unarchive", "delete"],
                },
                "name": str,
                "about": str,
                "visibility": { "type": "string", "enum": ["open", "private"] },
                "type": { "type": "string", "enum": ["stream", "forum"] },
                "ttl": {
                    "type": "integer",
                    "description": "Seconds without a message before it archives; 0: never.",
                },
                "topic": str,
                "purpose": str,
                "template": str,
            }),
            &[],
        ),
        tool(
            "members",
            "A channel's members; add or remove pubkeys or npubs, giving those added a role.",
            json!({
                "channel": str,
                "add": strs,
                "remove": strs,
                "role": { "type": "string", "enum": ROLES },
            }),
            &[],
        ),
        tool(
            "users",
            "Profiles by pubkey or name.",
            json!({ "ids": strs, "q": str }),
            &[],
        ),
        tool(
            "dm",
            "Open a DM; returns its channel.",
            json!({ "to": strs }),
            &["to"],
        ),
        tool(
            "mem_get",
            "Read memory; without slug, list slugs.",
            json!({ "slug": str }),
            &[],
        ),
        tool(
            "mem_set",
            "Write memory.",
            json!({ "slug": str, "text": str, "path": str }),
            &["slug"],
        ),
        tool(
            "canvas",
            "Read a channel's canvas, or replace it.",
            json!({ "channel": str, "text": str, "path": str }),
            &[],
        ),
        tool(
            "media_get",
            "See an image from a Buzz media URL.",
            json!({ "url": str }),
            &["url"],
        ),
    ]
}

const ROLES: [&str; 5] = ["owner", "admin", "member", "guest", "bot"];
/// The `channel` arguments each action takes.
const ACTIONS: [(&str, &[&str]); 8] = [
    ("details", &[]),
    (
        "create",
        &["name", "about", "visibility", "type", "ttl", "template"],
    ),
    (
        "update",
        &["name", "about", "visibility", "ttl", "topic", "purpose"],
    ),
    ("join", &[]),
    ("leave", &[]),
    ("archive", &[]),
    ("unarchive", &[]),
    ("delete", &[]),
];

/// Message kinds as the CLI reads them.
const MESSAGES: [u32; 5] = [9, 40002, 40008, 45001, 45003];
const THREAD: [u32; 4] = [9, 40002, 40008, 45003];
const SEARCH: [u32; 4] = [9, 40002, 45001, 45003];

fn thread_and_forum() -> Vec<u32> {
    let mut kinds = THREAD.to_vec();
    kinds.push(45001);
    kinds
}

pub(crate) fn hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Runs tool `name` for an agent working in `context`. An error is a tool
/// error for the agent to correct.
pub async fn call_tool(
    client: Arc<dyn BuzzClient>,
    context: &Context,
    name: &str,
    args: &Args,
) -> Result<Reply, String> {
    if !buzz_tool(name)
        && let Some(added) = super::added::get(name)
    {
        return (added.call)(client, context.clone(), args.clone()).await;
    }
    let tools = Tools {
        client,
        context: context.clone(),
        members: tokio::sync::Mutex::default(),
    };
    if name == "media_get" {
        return tools.media_get(args).await;
    }
    let text = match name {
        "send" => tools.send(args).await,
        "edit" => tools.edit(args).await,
        "react" => tools.react(args).await,
        "read" => tools.read(args).await,
        "channels" => tools.channels(args).await,
        "channel" => tools.channel(args).await,
        "members" => tools.members_tool(args).await,
        "users" => tools.users(args).await,
        "dm" => tools.dm(args).await,
        "mem_get" => tools.mem_get(args).await,
        "mem_set" => tools.mem_set(args).await,
        "canvas" => tools.canvas(args).await,
        _ => Err(format!("No tool {name}")),
    };
    text.map(Reply::Text)
}

/// One call's helpers, so lookups it repeats are made once.
struct Tools {
    client: Arc<dyn BuzzClient>,
    context: Context,
    members: tokio::sync::Mutex<HashMap<String, Vec<(String, String)>>>,
}

impl Tools {
    async fn query(&self, filters: Vec<Value>) -> Result<Vec<BuzzEvent>, String> {
        self.client.query(filters).await
    }

    async fn send(&self, args: &Args) -> ToolResult {
        let context = &self.context;
        let reply = text(args.get("reply")).or_else(|| {
            (!args.contains_key("channel"))
                .then(|| context.root.clone())
                .flatten()
        });
        // A reply goes to its message's channel.
        let mut channel = text(args.get("channel"));
        if channel.is_none()
            && let Some(reply) = &reply
            && Some(reply) != context.root.as_ref()
        {
            channel = tag(&self.event(reply).await?, "h");
        }
        let channel = channel
            .or_else(|| context.channel.clone())
            .ok_or("channel is required here.")?;
        if reply.is_some() && context.top_level && Some(&channel) == context.channel.as_ref() {
            return Err("This chat stays at the top level: send again without reply.".into());
        }
        let audience = args.get("audience").and_then(Value::as_str);
        if !matches!(audience, Some("everyone" | "agents")) {
            return Err("audience must be everyone or agents.".into());
        }
        let files = self.attach(&list(args.get("files"))).await?;
        let content = format!(
            "{}{}",
            self.body(args, files.links.is_empty())
                .await?
                .unwrap_or_default(),
            files.links
        );
        let thread = match &reply {
            Some(reply) => self.thread_tags(reply).await?,
            None => Vec::new(),
        };
        let mentions = self
            .mentions(&channel, &content, &list(args.get("mentions")))
            .await?;
        let mut tags = vec![vec!["h".to_string(), channel]];
        tags.extend(thread);
        tags.extend(mentions.into_iter().map(|key| vec!["p".to_string(), key]));
        tags.extend(files.tags);
        if audience == Some("agents") {
            tags.push(vec!["audience".into(), "agents".into()]);
        }
        let event = self
            .client
            .publish(Template {
                kind: 9,
                content,
                tags,
            })
            .await?;
        Ok(format!("sent {}", event.id))
    }

    async fn media_get(&self, args: &Args) -> Result<Reply, String> {
        use base64::Engine;
        let url = required(args, "url")?;
        let data = self.client.media(url).await?;
        // The first 12 bytes name the type.
        let head = base64::engine::general_purpose::STANDARD
            .decode(data.get(..16).unwrap_or(&data))
            .unwrap_or_default();
        match sniff(&head) {
            Some(mime) if mime.starts_with("image/") => Ok(Reply::Image {
                data,
                mime: mime.into(),
            }),
            _ => Err("Only JPEG, PNG, GIF and WebP images can be shown.".into()),
        }
    }

    async fn edit(&self, args: &Args) -> ToolResult {
        let id = required(args, "id")?;
        let target = self.event(&id).await?;
        let channel = tag(&target, "h").ok_or_else(|| format!("{id} is not a channel message."))?;
        let content = required(args, "text")?;
        // An edit replaces the attachments too, so it keeps the message's.
        let mut tags = vec![vec!["h".into(), channel], vec!["e".into(), id]];
        tags.extend(
            target
                .tags
                .iter()
                .filter(|tag| tag.first().map(String::as_str) == Some("imeta"))
                .cloned(),
        );
        self.client
            .publish(Template {
                kind: 40003,
                content,
                tags,
            })
            .await?;
        Ok("edited".into())
    }

    async fn react(&self, args: &Args) -> ToolResult {
        let id = required(args, "id")?;
        let emoji = required(args, "emoji")?;
        if args.get("off") != Some(&Value::Bool(true)) {
            self.client
                .publish(Template {
                    kind: 7,
                    content: emoji,
                    tags: vec![vec!["e".into(), id]],
                })
                .await?;
            return Ok("reacted".into());
        }
        let mine = self
            .query(vec![json!({
                "kinds": [7], "#e": [id], "authors": [self.client.pubkey()], "limit": 100,
            })])
            .await?;
        let Some(reaction) = mine.iter().find(|event| event.content == emoji) else {
            return Ok("no such reaction".into());
        };
        self.client
            .publish(Template {
                kind: 5,
                content: String::new(),
                tags: vec![vec!["e".into(), reaction.id.clone()]],
            })
            .await?;
        Ok("removed".into())
    }

    async fn read(&self, args: &Args) -> ToolResult {
        let context = &self.context;
        let limit = args.get("limit").and_then(Value::as_f64);
        let before = args.get("before").and_then(Value::as_f64);
        let q = text(args.get("q"));
        let mut channel = text(args.get("channel")).or_else(|| context.channel.clone());
        let named_channel = truthy(args.get("channel"));
        let events = if let Some(q) = q {
            let mut filter =
                json!({ "kinds": SEARCH, "search": q, "limit": bound(limit, 20, 100) });
            if named_channel {
                filter["#h"] = json!([channel]);
            }
            if let Some(before) = before.filter(|before| *before != 0.0) {
                filter["until"] = json!(before);
            }
            self.query(vec![filter]).await?
        } else if let Some(id) = text(args.get("thread")).or_else(|| {
            (!args.contains_key("channel"))
                .then(|| context.root.clone())
                .flatten()
        }) {
            let mut root = id.clone();
            if Some(&id) != context.root.as_ref() {
                let event = self.event(&id).await?;
                root = root_of(&event);
                channel = tag(&event, "h").or(channel);
            }
            let channel = channel.ok_or("channel is required here.")?;
            self.query(vec![
                json!({ "kinds": THREAD, "#h": [channel], "#e": [root], "limit": bound(limit, 100, 500) }),
                json!({ "ids": [root], "kinds": thread_and_forum(), "limit": 1 }),
            ])
            .await?
        } else {
            let channel = channel.ok_or("channel is required here.")?;
            let mut filter =
                json!({ "kinds": MESSAGES, "#h": [channel], "limit": bound(limit, 30, 200) });
            if let Some(before) = before.filter(|before| *before != 0.0) {
                filter["until"] = json!(before);
            }
            self.query(vec![filter]).await?
        };
        // An edit tags only the message it changes, so a thread's are not in it.
        let ids: Vec<&str> = events
            .iter()
            .filter(|event| event.kind != 40003)
            .map(|event| event.id.as_str())
            .collect();
        let edits = if ids.is_empty() {
            Vec::new()
        } else {
            self.query(vec![json!({ "kinds": [40003], "#e": ids, "limit": 500 })])
                .await?
        };
        let mut all = events;
        all.extend(edits);
        let messages = edited(all);
        if messages.is_empty() {
            return Ok("No messages.".into());
        }
        let names = self
            .names(messages.iter().map(|event| event.pubkey.clone()).collect())
            .await?;
        Ok(messages
            .iter()
            .map(|event| {
                format!(
                    "{} {} {}: {}",
                    event.id,
                    names.get(&event.pubkey).unwrap_or(&event.pubkey),
                    minute(event.created_at),
                    event.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn channels(&self, args: &Args) -> ToolResult {
        let memberships = self
            .query(vec![
                json!({ "kinds": [39002], "#p": [self.client.pubkey()], "limit": 500 }),
            ])
            .await?;
        let mut ids: Vec<String> = Vec::new();
        for id in memberships.iter().filter_map(|event| tag(event, "d")) {
            if !id.is_empty() && !ids.contains(&id) {
                ids.push(id);
            }
        }
        let all = args.get("all") == Some(&Value::Bool(true));
        if ids.is_empty() && !all {
            return Ok("No channels.".into());
        }
        let channels = self
            .query(vec![if all {
                json!({ "kinds": [39000], "limit": 500 })
            } else {
                json!({ "kinds": [39000], "#d": ids, "limit": ids.len() })
            }])
            .await?;
        let q = text(args.get("q")).map(|q| q.to_lowercase());
        let rows: Vec<String> = channels
            .iter()
            .map(|event| {
                let mut flags = Vec::new();
                match tag(event, "t") {
                    Some(kind) if kind != "stream" && !kind.is_empty() => flags.push(kind),
                    _ => {}
                }
                if has(event, "private") {
                    flags.push("private".into());
                }
                if has(event, "archived") {
                    flags.push("archived".into());
                }
                if !ids.contains(&tag(event, "d").unwrap_or_default()) {
                    flags.push("not joined".into());
                }
                format!(
                    "{} {}{}",
                    tag(event, "d").unwrap_or_default(),
                    tag(event, "name").unwrap_or_default(),
                    if flags.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", flags.join(", "))
                    }
                )
            })
            .filter(|row| q.as_ref().is_none_or(|q| row.to_lowercase().contains(q)))
            .collect();
        Ok(or(rows.join("\n"), "No channels."))
    }

    async fn channel(&self, args: &Args) -> ToolResult {
        let action = match args.get("action") {
            None => "details",
            Some(Value::String(action)) => action.as_str(),
            Some(_) => "",
        };
        let Some((_, fields)) = ACTIONS.iter().find(|(name, _)| *name == action) else {
            let names: Vec<&str> = ACTIONS.iter().map(|(name, _)| *name).collect();
            return Err(format!("action must be one of {}.", names.join(", ")));
        };
        let extra: Vec<&str> = args
            .keys()
            .map(String::as_str)
            .filter(|key| *key != "channel" && *key != "action" && !fields.contains(key))
            .collect();
        if !extra.is_empty() {
            return Err(format!(
                "{} cannot be used with {action}.",
                extra.join(", ")
            ));
        }
        if action == "create" {
            if args.contains_key("channel") {
                return Err("create makes a new channel; leave out channel.".into());
            }
            return self.create(args).await;
        }
        let channel = self.target(args)?;
        let lifecycle = match action {
            "details" => return self.details(&channel).await,
            "update" => return self.update(&channel, args).await,
            "join" => Action::Join,
            "leave" => Action::Leave,
            "archive" => Action::Archive,
            "unarchive" => Action::Unarchive,
            _ => Action::Delete,
        };
        let template = lifecycle::template(lifecycle, &channel, 0).map_err(|error| error.0)?;
        self.client
            .publish(Template {
                kind: u32::from(template.kind),
                content: template.content,
                tags: template.tags,
            })
            .await?;
        // The relay can accept a change it then fails to make, so it is
        // reported only once the channel shows it.
        let me = self.client.pubkey();
        let shown = until(
            || async {
                Ok::<_, String>(match lifecycle {
                    Action::Join | Action::Leave => {
                        self.roster(&channel)
                            .await?
                            .iter()
                            .any(|(pubkey, _)| *pubkey == me)
                            == (lifecycle == Action::Join)
                    }
                    Action::Delete => self.metadata(&channel).await?.is_none(),
                    _ => self.metadata(&channel).await?.is_some_and(|metadata| {
                        has(&metadata, "archived") == (lifecycle == Action::Archive)
                    }),
                })
            },
            |shown| *shown,
        )
        .await;
        let done = match lifecycle {
            Action::Join => "joined",
            Action::Leave => "left",
            Action::Archive => "archived",
            Action::Unarchive => "unarchived",
            _ => "deleted",
        };
        Ok(match shown {
            Ok(true) => format!("{done} {channel}"),
            Ok(false) => format!("The community accepted {action} but does not show it yet."),
            Err(error) => format!("The community accepted {action} but {}.", unread(&error)),
        })
    }

    /// The `channel` argument, or the current channel only when it is left
    /// out.
    fn target(&self, args: &Args) -> Result<String, String> {
        match args.get("channel") {
            None => self
                .context
                .channel
                .clone()
                .ok_or_else(|| "channel is required here.".to_string()),
            value => text(value).ok_or_else(|| "channel must be a channel ID.".to_string()),
        }
    }

    async fn members_tool(&self, args: &Args) -> ToolResult {
        let channel = self.target(args)?;
        let add = keys(args, "add")?;
        let remove = keys(args, "remove")?;
        let role = match args.get("role") {
            None => None,
            Some(Value::String(role)) if ROLES.contains(&role.as_str()) => Some(role.clone()),
            Some(_) => return Err(format!("role must be one of {}.", ROLES.join(", "))),
        };
        if role.is_some() && add.is_empty() {
            return Err("role is for the members you add.".into());
        }
        if !add.is_empty() || !remove.is_empty() {
            lifecycle::channel_id(&channel).map_err(|error| error.0)?;
            let me = self.client.pubkey();
            if add.iter().chain(&remove).any(|key| *key == me) {
                return Err("For yourself, use channel with action join or leave.".into());
            }
            if add.iter().any(|key| remove.contains(key)) {
                return Err("Add or remove someone, not both.".into());
            }
            if add.len() + remove.len() > 50 {
                return Err("Change at most 50 members at a time.".into());
            }
            let (lines, changed) = self.change(&channel, &add, &remove, role.as_deref()).await;
            return if changed {
                Ok(lines.join("\n"))
            } else {
                Err(lines.join("\n"))
            };
        }
        let members = self.members(&channel).await?;
        let names = self
            .names(members.iter().map(|(pubkey, _)| pubkey.clone()).collect())
            .await?;
        Ok(members
            .iter()
            .map(|(pubkey, role)| {
                format!(
                    "{pubkey} {}{}",
                    names.get(pubkey).map(String::as_str).unwrap_or(""),
                    if role == "member" {
                        String::new()
                    } else {
                        format!(" ({role})")
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn users(&self, args: &Args) -> ToolResult {
        let ids = list(args.get("ids"))
            .iter()
            .map(|id| pubkey(id))
            .collect::<Result<Vec<_>, _>>()?;
        let q = text(args.get("q")).map(|q| q.to_lowercase());
        let filter = if !ids.is_empty() {
            json!({ "kinds": [0], "authors": ids, "limit": ids.len() })
        } else if let Some(q) = &q {
            json!({ "kinds": [0], "search": q, "limit": 100 })
        } else {
            return Err("Give ids or q.".into());
        };
        let profiles = self.query(vec![filter]).await?;
        let rows: Vec<String> = newest(profiles)
            .iter()
            .filter_map(|event| {
                let profile = parse(&event.content);
                let name = profile_name(&profile).unwrap_or_default();
                let about = profile
                    .get("about")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|about| !about.is_empty())
                    .map(|about| format!(" — {}", about.chars().take(120).collect::<String>()))
                    .unwrap_or_default();
                let keep = q
                    .as_ref()
                    .is_none_or(|q| !ids.is_empty() || name.to_lowercase().contains(q.as_str()));
                keep.then(|| format!("{} {name}{about}", event.pubkey))
            })
            .collect();
        Ok(or(rows.join("\n"), "No users."))
    }

    async fn dm(&self, args: &Args) -> ToolResult {
        let to = list(args.get("to"))
            .iter()
            .map(|id| pubkey(id))
            .collect::<Result<Vec<_>, _>>()?;
        if to.is_empty() {
            return Err("to is required.".into());
        }
        let me = self.client.pubkey();
        let people: HashSet<String> = std::iter::once(me.clone()).chain(to.clone()).collect();
        let find = || async {
            let events = self
                .query(vec![
                    json!({ "kinds": [39000], "#p": [me], "#t": ["dm"], "limit": 500 }),
                ])
                .await?;
            Ok::<_, String>(events.into_iter().find(|event| {
                let members: HashSet<&str> = event
                    .tags
                    .iter()
                    .filter(|t| t.first().map(String::as_str) == Some("p"))
                    .filter_map(|t| t.get(1).map(String::as_str))
                    .collect();
                members.len() == people.len()
                    && people.iter().all(|key| members.contains(key.as_str()))
            }))
        };
        let mut found = find().await?;
        if found.is_none() {
            let mut tags: Vec<Vec<String>> =
                to.iter().map(|key| vec!["p".into(), key.clone()]).collect();
            tags.push(vec!["d".into(), uuid::Uuid::new_v4().to_string()]);
            self.client
                .publish(Template {
                    kind: 41010,
                    content: String::new(),
                    tags,
                })
                .await?;
            // The relay creates the channel's metadata just after answering.
            for tries in 0..5 {
                if found.is_some() {
                    break;
                }
                if tries > 0 {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                found = find().await?;
            }
        }
        found
            .and_then(|event| tag(&event, "d"))
            .filter(|channel| !channel.is_empty())
            .ok_or_else(|| "The DM did not open.".into())
    }

    async fn mem_get(&self, args: &Args) -> ToolResult {
        let entries = self.client.memories().await?;
        let Some(slug) = text(args.get("slug")) else {
            let rows: Vec<String> = entries
                .iter()
                .map(|entry| format!("{} {}", entry.slug, minute(entry.created_at)))
                .collect();
            return Ok(or(rows.join("\n"), "No memory."));
        };
        let key = memory_slug(&slug)?;
        Ok(entries
            .into_iter()
            .find(|entry| entry.slug == key)
            .map(|entry| entry.body)
            .unwrap_or_else(|| format!("No memory {key}.")))
    }

    async fn mem_set(&self, args: &Args) -> ToolResult {
        let key = memory_slug(&required(args, "slug")?)?;
        let body = self.body(args, true).await?.unwrap_or_default();
        // Unreadable memory still takes a write, dated now.
        let after = self
            .client
            .memories()
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|entry| entry.slug == key)
            .map_or(0, |entry| entry.created_at);
        self.client.remember(key, body, after).await?;
        Ok("saved".into())
    }

    async fn canvas(&self, args: &Args) -> ToolResult {
        let channel = text(args.get("channel"))
            .or_else(|| self.context.channel.clone())
            .ok_or("channel is required here.")?;
        let content = self.body(args, false).await?;
        let head = self
            .query(vec![
                json!({ "kinds": [40100], "#h": [channel], "limit": 1 }),
            ])
            .await?
            .into_iter()
            .next();
        let Some(content) = content else {
            return Ok(head
                .map(|head| head.content)
                .filter(|content| !content.is_empty())
                .unwrap_or_else(|| "No canvas.".into()));
        };
        // The newest canvas wins, so a write must be dated after the current one.
        let wait = head.map_or(0, |head| {
            (head.created_at as i64 + 1) * 1000 - chrono::Utc::now().timestamp_millis()
        });
        if wait > 5000 {
            return Err("The canvas was just changed; try again.".into());
        }
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait as u64)).await;
        }
        self.client
            .publish(Template {
                kind: 40100,
                content,
                tags: vec![vec!["h".into(), channel]],
            })
            .await?;
        Ok("saved".into())
    }

    /// `text`, or the file at `path`. `None` when neither is given and
    /// `needed` is false.
    async fn body(&self, args: &Args, needed: bool) -> Result<Option<String>, String> {
        let value = if let Some(path) = text(args.get("path")) {
            let bytes = self.client.read(path).await?;
            let text = String::from_utf8_lossy(&bytes);
            Some(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
        } else {
            args.get("text").and_then(Value::as_str).map(str::to_string)
        };
        if value.is_none() && needed {
            return Err("Give text or path.".into());
        }
        Ok(value)
    }

    async fn event(&self, id: &str) -> Result<BuzzEvent, String> {
        if !hex(id) {
            return Err(format!("Not an event id: {id}"));
        }
        self.query(vec![
            json!({ "ids": [id], "kinds": thread_and_forum(), "limit": 1 }),
        ])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| format!("No message {id}."))
    }

    /// The e-tags of a reply to `parent`: one `reply` tag for a reply to a
    /// thread's root, `root` and `reply` for one nested deeper.
    /// The e-tag of a reply to `parent`. Agents never nest: a reply to any
    /// message in a thread is a reply to the thread's root.
    async fn thread_tags(&self, parent: &str) -> Result<Vec<Vec<String>>, String> {
        let root = if Some(parent) == self.context.root.as_deref() {
            parent.to_string()
        } else {
            root_of(&self.event(parent).await?)
        };
        Ok(vec![vec!["e".into(), root, String::new(), "reply".into()]])
    }

    /// The channel's members and their roles, read once per call.
    async fn members(&self, channel: &str) -> Result<Vec<(String, String)>, String> {
        if let Some(members) = self.members.lock().await.get(channel) {
            return Ok(members.clone());
        }
        let members = self.roster(channel).await?;
        self.members
            .lock()
            .await
            .insert(channel.into(), members.clone());
        Ok(members)
    }

    /// The channel's metadata now, if the agent can see it.
    async fn metadata(&self, channel: &str) -> Result<Option<BuzzEvent>, String> {
        Ok(self
            .query(vec![
                json!({ "kinds": [39000], "#d": [channel], "limit": 1 }),
            ])
            .await?
            .into_iter()
            .next())
    }

    async fn details(&self, channel: &str) -> ToolResult {
        let metadata = self
            .metadata(channel)
            .await?
            .ok_or_else(|| format!("No channel {channel} that you can see."))?;
        let me = self.client.pubkey();
        let role = self
            .roster(channel)
            .await?
            .into_iter()
            .find(|(pubkey, _)| *pubkey == me)
            .map(|(_, role)| role);
        let mut flags = vec![tag(&metadata, "t").unwrap_or_default()];
        flags.push(
            if has(&metadata, "private") {
                "private"
            } else {
                "open"
            }
            .into(),
        );
        if has(&metadata, "archived") {
            flags.push("archived".into());
        }
        if let Some(ttl) = tag(&metadata, "ttl").filter(|ttl| !ttl.is_empty()) {
            flags.push(format!("archives after {ttl}s idle"));
        }
        flags.retain(|flag| !flag.is_empty());
        let mut lines = vec![format!(
            "{channel} {} ({})",
            tag(&metadata, "name").unwrap_or_default(),
            flags.join(", ")
        )];
        for key in ["about", "topic", "purpose"] {
            if let Some(value) = tag(&metadata, key).filter(|value| !value.is_empty()) {
                lines.push(format!("{key}: {value}"));
            }
        }
        lines.push(format!(
            "you: {}",
            role.as_deref().unwrap_or("not a member")
        ));
        Ok(lines.join("\n"))
    }

    async fn create(&self, args: &Args) -> ToolResult {
        let name = lifecycle::canonical_name(&required(args, "name")?);
        let about = optional(args, "about")?.filter(|about| !about.is_empty());
        let ttl = seconds(args.get("ttl"))?;
        check_details(Some(&name), about.as_deref(), ttl)?;
        let visibility = match args.get("visibility").and_then(Value::as_str) {
            Some(visibility @ ("open" | "private")) => visibility,
            _ => return Err("visibility must be open or private.".into()),
        };
        let kind = match args.get("type") {
            None => "stream",
            Some(Value::String(kind)) if kind == "stream" || kind == "forum" => kind.as_str(),
            Some(_) => return Err("type must be stream or forum.".into()),
        };
        // Looked for first, so a wrong name creates nothing.
        let template = match args.contains_key("template") {
            true => Some(self.template(&required(args, "template")?).await?),
            false => None,
        };
        let id = uuid::Uuid::new_v4().to_string();
        let mut tags = vec![
            vec!["h".to_string(), id.clone()],
            vec!["name".into(), name],
            vec!["visibility".into(), visibility.into()],
            vec!["channel_type".into(), kind.into()],
        ];
        if let Some(about) = about {
            tags.push(vec!["about".into(), about]);
        }
        if let Some(ttl) = ttl {
            tags.push(vec!["ttl".into(), ttl.to_string()]);
        }
        self.client
            .publish(Template {
                kind: 9007,
                content: String::new(),
                tags,
            })
            .await?;
        let shown = until(|| self.metadata(&id), Option::is_some).await;
        let unapplied = if template.is_some() {
            "; the template was not applied"
        } else {
            ""
        };
        match shown {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Ok(format!(
                    "The community accepted channel {id} but does not show it yet{unapplied}."
                ));
            }
            Err(error) => {
                return Ok(format!(
                    "The community accepted channel {id} but {}{unapplied}.",
                    unread(&error)
                ));
            }
        }
        let mut lines = vec![format!("created {id}")];
        let Some(template) = template else {
            return Ok(lines.join("\n"));
        };
        // The template's own agents, as Buzz adds them; each is tried.
        let me = self.client.pubkey();
        let agents: Vec<String> = template
            .agents
            .into_iter()
            .filter(|key| *key != me)
            .collect();
        if !template.canvas.trim().is_empty() {
            let started = self
                .client
                .publish(Template {
                    kind: 40100,
                    content: template.canvas,
                    tags: vec![
                        vec!["h".into(), id.clone()],
                        vec!["expected-revision".into(), "none".into()],
                    ],
                })
                .await;
            match started {
                Ok(_) => lines.push("started the Canvas".into()),
                Err(error) => {
                    // As in Buzz, agents join only once the template's Canvas
                    // is started.
                    let skipped = if agents.is_empty() {
                        ""
                    } else {
                        ", so no agents were added"
                    };
                    lines.push(format!("Canvas not started{skipped}: {error}"));
                    return Ok(lines.join("\n"));
                }
            }
        }
        if !agents.is_empty() {
            lines.extend(self.change(&id, &agents, &[], None).await.0);
        }
        Ok(lines.join("\n"))
    }

    /// The owner's template named `name`, as one, without guessing.
    async fn template(&self, name: &str) -> Result<super::ChannelTemplate, String> {
        let Some(templates) = self.client.templates() else {
            return Err(
                "Channel templates are kept in Buzz Desktop and are unavailable here.".into(),
            );
        };
        let templates = templates.await?;
        let wanted = name.trim().to_lowercase();
        let mut found: Vec<super::ChannelTemplate> = templates
            .iter()
            .filter(|template| template.name.to_lowercase() == wanted)
            .cloned()
            .collect();
        if found.len() > 1 {
            return Err(format!(
                "{} templates are named {name}; your owner should rename one.",
                found.len()
            ));
        }
        found.pop().ok_or_else(|| {
            let known = if templates.is_empty() {
                "Your owner has none.".to_string()
            } else {
                let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
                format!("Templates: {}.", names.join(", "))
            };
            format!("No template named {name}. {known}")
        })
    }

    async fn update(&self, channel: &str, args: &Args) -> ToolResult {
        let metadata = self
            .metadata(channel)
            .await?
            .ok_or_else(|| format!("No channel {channel} that you can see."))?;
        let mut wanted: Vec<(String, String)> = Vec::new();
        if args.contains_key("name") {
            wanted.push((
                "name".into(),
                lifecycle::canonical_name(&required(args, "name")?),
            ));
        }
        for key in ["about", "topic", "purpose"] {
            if let Some(value) = optional(args, key)? {
                wanted.push((key.into(), value));
            }
        }
        if let Some(visibility) = args.get("visibility") {
            match visibility.as_str() {
                Some(visibility @ ("open" | "private")) => {
                    wanted.push(("visibility".into(), visibility.into()))
                }
                _ => return Err("visibility must be open or private.".into()),
            }
        }
        if args.contains_key("ttl") {
            let ttl = seconds(args.get("ttl"))?;
            wanted.push((
                "ttl".into(),
                ttl.map(|ttl| ttl.to_string()).unwrap_or_default(),
            ));
        }
        if wanted.is_empty() {
            return Err("Give name, about, visibility, ttl, topic or purpose.".into());
        }
        let value = |key: &str| {
            wanted
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        check_details(
            value("name").as_deref(),
            value("about").as_deref(),
            value("ttl")
                .and_then(|ttl| ttl.parse().ok())
                .filter(|ttl| *ttl > 0),
        )?;
        if buzz_relay_client::discovery::session_metadata(tag(&metadata, "about").as_deref())
            .is_some()
            && wanted.iter().any(|(key, _)| key != "name")
        {
            return Err("Only the name of a session can be changed.".into());
        }
        let id = lifecycle::channel_id(channel).map_err(|error| error.0)?;
        let mut tags = vec![vec!["h".to_string(), id.to_string()]];
        tags.extend(
            wanted
                .iter()
                .map(|(key, value)| vec![key.clone(), value.clone()]),
        );
        self.client
            .publish(Template {
                kind: 9002,
                content: String::new(),
                tags,
            })
            .await?;
        let shows = |now: &Option<BuzzEvent>| {
            now.as_ref().is_some_and(|now| {
                wanted.iter().all(|(key, value)| {
                    if key == "visibility" {
                        has(now, "private") == (value == "private")
                    } else {
                        tag(now, key).unwrap_or_default() == *value
                    }
                })
            })
        };
        Ok(match until(|| self.metadata(channel), shows).await {
            Ok(now) if shows(&now) => format!("updated {channel}"),
            Ok(_) => "The community accepted the update but does not show it yet.".into(),
            Err(error) => format!("The community accepted the update but {}.", unread(&error)),
        })
    }

    /// Adds and removes members, each tried on its own, and reports each as
    /// the roster shows it, and whether any was accepted.
    async fn change(
        &self,
        channel: &str,
        add: &[String],
        remove: &[String],
        role: Option<&str>,
    ) -> (Vec<String>, bool) {
        let mut refused: HashMap<String, String> = HashMap::new();
        for (targets, kind) in [(add, 9000), (remove, 9001)] {
            for key in targets {
                let mut tags = vec![
                    vec!["h".to_string(), channel.to_string()],
                    vec!["p".into(), key.clone()],
                ];
                if let (9000, Some(role)) = (kind, role) {
                    tags.push(vec!["role".into(), role.into()]);
                }
                let published = self
                    .client
                    .publish(Template {
                        kind,
                        content: String::new(),
                        tags,
                    })
                    .await;
                if let Err(error) = published {
                    refused.insert(key.clone(), error);
                }
            }
        }
        let added = |roster: &[(String, String)], key: &str| {
            roster
                .iter()
                .any(|(pubkey, held)| pubkey == key && role.is_none_or(|role| held == role))
        };
        let removed = |roster: &[(String, String)], key: &str| {
            !roster.iter().any(|(pubkey, _)| pubkey == key)
        };
        let roster = until(
            || self.roster(channel),
            |now: &Vec<(String, String)>| {
                add.iter()
                    .all(|key| refused.contains_key(key) || added(now, key))
                    && remove
                        .iter()
                        .all(|key| refused.contains_key(key) || removed(now, key))
            },
        )
        .await;
        let names = self
            .names(add.iter().chain(remove).cloned().collect())
            .await
            .unwrap_or_default();
        let who = |key: &str| match names.get(key) {
            Some(name) => format!("{key} {name}"),
            None => key.to_string(),
        };
        let report = |key: &str, verb: &str, done: &Shown<'_>| {
            if let Some(reason) = refused.get(key) {
                return format!("not {verb} {}: {reason}", who(key));
            }
            match &roster {
                Err(error) => format!(
                    "{}: the community accepted it but {}",
                    who(key),
                    unread(error)
                ),
                Ok(now) if done(now, key) => format!(
                    "{verb} {}{}",
                    who(key),
                    match role {
                        Some(role) if verb == "added" => format!(" as {role}"),
                        _ => String::new(),
                    }
                ),
                Ok(_) => format!(
                    "{}: the community accepted it but does not show it yet",
                    who(key)
                ),
            }
        };
        let lines = add
            .iter()
            .map(|key| report(key, "added", &added))
            .chain(remove.iter().map(|key| report(key, "removed", &removed)))
            .collect();
        (lines, refused.len() < add.len() + remove.len())
    }

    /// The channel's members now, uncached.
    async fn roster(&self, channel: &str) -> Result<Vec<(String, String)>, String> {
        let list = self
            .query(vec![
                json!({ "kinds": [39002], "#d": [channel], "limit": 1 }),
            ])
            .await?;
        let members: Vec<(String, String)> = list
            .first()
            .map(|list| {
                list.tags
                    .iter()
                    .filter(|t| t.first().map(String::as_str) == Some("p"))
                    .filter_map(|t| {
                        let key = t.get(1).filter(|key| !key.is_empty())?;
                        let role = t
                            .get(3)
                            .filter(|role| !role.is_empty())
                            .cloned()
                            .unwrap_or_else(|| "member".into());
                        Some((key.clone(), role))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(members)
    }

    /// Display names of `pubkeys` that have a profile.
    async fn names(&self, pubkeys: Vec<String>) -> Result<HashMap<String, String>, String> {
        let mut wanted: Vec<String> = Vec::new();
        for key in pubkeys {
            if !wanted.contains(&key) {
                wanted.push(key);
            }
        }
        let mut names = HashMap::new();
        if wanted.is_empty() {
            return Ok(names);
        }
        let profiles = self
            .query(vec![
                json!({ "kinds": [0], "authors": wanted, "limit": wanted.len() }),
            ])
            .await?;
        for event in newest(profiles) {
            if let Some(name) = profile_name(&parse(&event.content)) {
                names.insert(event.pubkey.clone(), name);
            }
        }
        Ok(names)
    }

    /// Who `content` notifies: `mentions`, `nostr:npub…` links, and `@Name`
    /// for a channel member, as the CLI resolves them. Each must be a member.
    async fn mentions(
        &self,
        channel: &str,
        content: &str,
        explicit: &[String],
    ) -> Result<Vec<String>, String> {
        let mut keys: Vec<String> = Vec::new();
        let add = |key: String, keys: &mut Vec<String>| {
            if !keys.contains(&key) {
                keys.push(key);
            }
        };
        for key in explicit {
            add(pubkey(key)?, &mut keys);
        }
        for npub in npub_links(content) {
            add(pubkey(&npub)?, &mut keys);
        }
        let prose = strip_code(content);
        if !prose.contains('@') && keys.is_empty() {
            return Ok(Vec::new());
        }
        let members = self.members(channel).await?;
        if prose.contains('@') {
            let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
            let names = self
                .names(members.iter().map(|(key, _)| key.clone()).collect())
                .await?;
            // In member order, as buzz-app's Map iterates.
            for (key, _) in &members {
                if let Some(name) = names.get(key) {
                    by_name
                        .entry(name.to_lowercase())
                        .or_default()
                        .push(key.clone());
                }
            }
            let mut longest: Vec<&String> = by_name.keys().collect();
            longest.sort_by_key(|name| std::cmp::Reverse(name.chars().count()));
            let mut unknown = Vec::new();
            for start in at_signs(&prose) {
                let rest = prose[start..].to_lowercase();
                let name = longest.iter().find(|candidate| {
                    rest.starts_with(candidate.as_str())
                        && rest[candidate.len()..]
                            .chars()
                            .next()
                            .is_none_or(|next| next.is_whitespace() || ",;.!?:)]}".contains(next))
                });
                let found = name.and_then(|name| by_name.get(*name));
                match found {
                    Some(found) if found.len() == 1 => add(found[0].clone(), &mut keys),
                    _ => {
                        let token = name.map(|name| name.to_string()).or_else(|| {
                            let token: String = rest
                                .chars()
                                .take_while(|c| {
                                    c.is_ascii_lowercase()
                                        || c.is_ascii_digit()
                                        || matches!(c, '.' | '_' | '-')
                                })
                                .collect();
                            (!token.is_empty()).then_some(token)
                        });
                        if let Some(token) = token {
                            let ambiguous = if found.is_some_and(|found| !found.is_empty()) {
                                " (ambiguous)"
                            } else {
                                ""
                            };
                            unknown.push(format!("@{token}{ambiguous}"));
                        }
                    }
                }
            }
            if !unknown.is_empty() && explicit.is_empty() {
                return Err(format!(
                    "No single channel member is {}. Pass their pubkeys in mentions, or drop the @.",
                    unknown.join(", ")
                ));
            }
        }
        let outside: Vec<&String> = keys
            .iter()
            .filter(|key| !members.iter().any(|(member, _)| member == *key))
            .collect();
        if !outside.is_empty() {
            return Err(format!(
                "Not in this channel: {}",
                outside
                    .iter()
                    .map(|key| key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        keys.truncate(50);
        Ok(keys)
    }

    /// Uploads each file and returns its `imeta` tags and the links to append.
    async fn attach(&self, paths: &[String]) -> Result<Attached, String> {
        let mut attached = Attached::default();
        for path in paths {
            let data = self.client.read(path.clone()).await?;
            let mime = sniff(&data)
                .ok_or_else(|| format!("{path}: only JPEG, PNG, GIF, WebP and MP4 attach."))?;
            let upload = self.client.upload(data, mime.into()).await?;
            attached.links += &format!(
                "\n![{}]({})",
                if mime.starts_with("video/") {
                    "video"
                } else {
                    "image"
                },
                upload.url
            );
            attached.tags.push(imeta(&upload));
        }
        Ok(attached)
    }
}

#[derive(Default)]
struct Attached {
    tags: Vec<Vec<String>>,
    links: String,
}

/// The root of the thread `event` is in, by its NIP-10 markers (the last
/// of each, as the relay reads them), or `event` itself.
pub(crate) fn root_of(event: &BuzzEvent) -> String {
    crate::agent_plugins::claude_code::runtime::thread_reference(event)
        .map_or_else(|| event.id.clone(), |thread| thread.root_id)
}

/// Messages oldest first, each showing its author's latest edit. Edits
/// by anyone else are left out before the newest is picked, so they can't
/// hide the author's own.
fn edited(events: Vec<BuzzEvent>) -> Vec<BuzzEvent> {
    let authors: HashMap<&str, &str> = events
        .iter()
        .filter(|event| event.kind != 40003)
        .map(|event| (event.id.as_str(), event.pubkey.as_str()))
        .collect();
    let mut edits: HashMap<String, &BuzzEvent> = HashMap::new();
    for event in events.iter().filter(|event| event.kind == 40003) {
        if let Some(target) = tag(event, "e").filter(|target| !target.is_empty()) {
            if authors.get(target.as_str()) != Some(&event.pubkey.as_str()) {
                continue;
            }
            let newer = edits
                .get(&target)
                .is_none_or(|last| last.created_at < event.created_at);
            if newer {
                edits.insert(target, event);
            }
        }
    }
    let mut messages: Vec<BuzzEvent> = events
        .iter()
        .filter(|event| event.kind != 40003)
        .map(|event| match edits.get(&event.id) {
            Some(edit) => BuzzEvent {
                content: edit.content.clone(),
                ..event.clone()
            },
            _ => event.clone(),
        })
        .collect();
    messages.sort_by_key(|event| event.created_at);
    messages
}

/// Each author's newest event, in order of first appearance.
fn newest(events: Vec<BuzzEvent>) -> Vec<BuzzEvent> {
    let mut latest: Vec<BuzzEvent> = Vec::new();
    for event in events {
        match latest.iter_mut().find(|seen| seen.pubkey == event.pubkey) {
            Some(seen) if seen.created_at < event.created_at => *seen = event,
            Some(_) => {}
            None => latest.push(event),
        }
    }
    latest
}

fn imeta(upload: &Upload) -> Vec<String> {
    let mut tag = vec![
        "imeta".to_string(),
        format!("url {}", upload.url),
        format!("m {}", upload.mime),
        format!("x {}", upload.sha256),
        format!("size {}", upload.size),
    ];
    for (key, value) in [
        ("dim", upload.dim.clone()),
        ("blurhash", upload.blurhash.clone()),
        ("thumb", upload.thumb.clone()),
        (
            "duration",
            upload.duration.map(|duration| duration.to_string()),
        ),
    ] {
        if let Some(value) = value {
            tag.push(format!("{key} {value}"));
        }
    }
    tag
}

/// An attachable type, by the file's leading bytes.
fn sniff(data: &[u8]) -> Option<&'static str> {
    let at = |offset: usize, bytes: &[u8]| data.get(offset..offset + bytes.len()) == Some(bytes);
    if at(0, &[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if at(0, &[0x89, 0x50, 0x4e, 0x47]) {
        Some("image/png")
    } else if at(0, &[0x47, 0x49, 0x46, 0x38]) {
        Some("image/gif")
    } else if at(0, &[0x52, 0x49, 0x46, 0x46]) && at(8, &[0x57, 0x45, 0x42, 0x50]) {
        Some("image/webp")
    } else if at(4, &[0x66, 0x74, 0x79, 0x70]) {
        Some("video/mp4")
    } else {
        None
    }
}

fn memory_slug(slug: &str) -> Result<String, String> {
    let key = if slug == "core" || slug.starts_with("mem/") {
        slug.to_string()
    } else {
        format!("mem/{slug}")
    };
    if !buzz_desktop::relay::agent::slug(&key) {
        return Err(format!(
            "Bad slug {slug}: use lowercase parts like notes or team/plan."
        ));
    }
    Ok(key)
}

fn pubkey(value: &str) -> Result<String, String> {
    let lower = value.to_lowercase();
    if hex(&lower) {
        return Ok(lower);
    }
    nostr::prelude::PublicKey::from_bech32(value)
        .map(|key| key.to_hex())
        .map_err(|_| format!("Not a pubkey: {value}"))
}

/// The `npub1…` of each `nostr:npub1…` link in `content`.
fn npub_links(content: &str) -> Vec<String> {
    const CHARSET: &str = "023456789acdefghjklmnpqrstuvwxyz";
    let mut links = Vec::new();
    let mut rest = content;
    while let Some(at) = rest.find("nostr:npub1") {
        let start = at + "nostr:".len();
        let body = &rest[start + 5..];
        let length = body.chars().take_while(|c| CHARSET.contains(*c)).count();
        if length >= 58 {
            links.push(rest[start..start + 5 + 58].to_string());
        }
        rest = &rest[start..];
        rest = &rest[5..];
    }
    links
}

/// `content` without fenced code blocks or inline code, which never notify.
fn strip_code(content: &str) -> String {
    // A fence opens at a line start and closes at the next line that starts
    // with one (buzz-app's /^```[\s\S]*?^```/gm).
    let starts: Vec<usize> = std::iter::once(0)
        .chain(content.match_indices('\n').map(|(index, _)| index + 1))
        .collect();
    let fence = |index: usize| content[starts[index]..].starts_with("```");
    let mut prose = String::new();
    let mut copied = 0;
    let mut index = 0;
    while index < starts.len() {
        if fence(index)
            && let Some(close) = (index + 1..starts.len()).find(|&close| fence(close))
        {
            prose += &content[copied..starts[index]];
            prose += " ";
            copied = starts[close] + 3;
            index = close + 1;
            continue;
        }
        index += 1;
    }
    prose += &content[copied..];
    // Inline code: a backtick, anything but backticks and newlines, a backtick.
    let mut out = String::new();
    let mut rest = prose.as_str();
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        match after.find(['`', '\n']) {
            Some(close) if after.as_bytes()[close] == b'`' => {
                out += &rest[..open];
                out += " ";
                rest = &after[close + 1..];
            }
            _ => {
                out += &rest[..=open];
                rest = after;
            }
        }
    }
    out += rest;
    out
}

/// Byte offsets just after each `@` that starts the text or follows
/// whitespace.
fn at_signs(prose: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut previous: Option<char> = None;
    for (index, c) in prose.char_indices() {
        if c == '@' && previous.is_none_or(char::is_whitespace) {
            starts.push(index + 1);
        }
        previous = Some(c);
    }
    starts
}

fn profile_name(profile: &Map<String, Value>) -> Option<String> {
    ["display_name", "name"]
        .iter()
        .filter_map(|key| profile.get(*key).and_then(Value::as_str))
        .find(|name| !name.trim().is_empty())
        .map(str::to_string)
}

fn parse(content: &str) -> Map<String, Value> {
    match serde_json::from_str(content) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

pub(crate) fn tag(event: &BuzzEvent, name: &str) -> Option<String> {
    event
        .tags
        .iter()
        .find(|t| t.first().map(String::as_str) == Some(name))
        .and_then(|t| t.get(1).cloned())
}

fn has(event: &BuzzEvent, name: &str) -> bool {
    event
        .tags
        .iter()
        .any(|t| t.first().map(String::as_str) == Some(name))
}

/// Loads until `done` holds, a few times, and returns the last load: the
/// relay updates a channel's state just after answering.
async fn until<T, F, L>(load: L, done: impl Fn(&T) -> bool) -> Result<T, String>
where
    L: Fn() -> F,
    F: std::future::Future<Output = Result<T, String>>,
{
    let mut value = load().await?;
    for _ in 1..5 {
        if done(&value) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        value = load().await?;
    }
    Ok(value)
}

/// A failed read after a write, as the reason the write is not confirmed.
fn unread(error: &str) -> String {
    format!("its state could not be read back: {error}")
}

/// A list of pubkeys or npubs, each once; any other entry is refused, not
/// dropped.
fn keys(args: &Args, name: &str) -> Result<Vec<String>, String> {
    let Some(value) = args.get(name) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .filter(|items| items.iter().all(Value::is_string))
        .ok_or_else(|| format!("{name} must be a list of pubkeys."))?;
    let mut keys: Vec<String> = Vec::new();
    for item in items {
        let key = pubkey(item.as_str().unwrap_or_default())?;
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

/// An optional text argument, trimmed; refused when it is not text.
fn optional(args: &Args, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.trim().to_string())),
        Some(_) => Err(format!("{key} must be text.")),
    }
}

/// A `ttl` argument: `None` when absent or 0, which means never.
fn seconds(value: Option<&Value>) -> Result<Option<u64>, String> {
    match value {
        None => Ok(None),
        Some(value) => match value.as_u64() {
            Some(0) => Ok(None),
            Some(ttl) => Ok(Some(ttl)),
            None => Err("ttl must be a whole number of seconds.".into()),
        },
    }
}

/// The checks Buzz makes on a channel's name, description and lifetime.
fn check_details(name: Option<&str>, about: Option<&str>, ttl: Option<u64>) -> Result<(), String> {
    let errors = lifecycle::draft_errors(name.unwrap_or("-"), about.unwrap_or(""), ttl);
    match errors.name.or(errors.description).or(errors.lifetime) {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// JavaScript truthiness, for arguments buzz-app tests that way.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

fn list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn required(args: &Args, name: &str) -> Result<String, String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{name} is required."))
}

fn bound(value: Option<f64>, fallback: u32, max: u32) -> u32 {
    let value = value.unwrap_or(f64::from(fallback)).floor();
    value.clamp(1.0, f64::from(max)) as u32
}

fn or(text: String, empty: &str) -> String {
    if text.is_empty() { empty.into() } else { text }
}

/// A time to the minute, as the CLI prints it.
fn minute(seconds: u64) -> String {
    chrono::DateTime::from_timestamp(seconds as i64, 0)
        .map(|time| time.format("%Y-%m-%dT%H:%MZ").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests;
