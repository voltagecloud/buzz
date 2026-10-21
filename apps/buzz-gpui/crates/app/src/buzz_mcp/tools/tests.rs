//! buzz-app's `src/buzz-mcp/tools.test.ts`, against a fake community.

use super::*;
use crate::buzz_mcp::rpc::respond;
use crate::buzz_mcp::{ChannelTemplate, ClientResult, Memory};
use futures::future::BoxFuture;
use std::sync::Mutex;

const PNG: [u8; 7] = [0x89, 0x50, 0x4e, 0x47, 1, 2, 3];

fn me() -> String {
    "c".repeat(64)
}
fn alice() -> String {
    "a".repeat(64)
}
fn bob() -> String {
    "b".repeat(64)
}
fn root() -> String {
    "1".repeat(64)
}
fn nested() -> String {
    "2".repeat(64)
}

fn event(change: impl FnOnce(&mut BuzzEvent)) -> BuzzEvent {
    let mut event = BuzzEvent {
        id: "f".repeat(64),
        pubkey: alice(),
        created_at: 100,
        kind: 9,
        tags: vec![vec!["h".into(), "chan".into()]],
        content: String::new(),
        sig: String::new(),
    };
    change(&mut event);
    event
}

fn tags(list: &[&[&str]]) -> Vec<Vec<String>> {
    list.iter()
        .map(|tag| tag.iter().map(|part| part.to_string()).collect())
        .collect()
}

/// A community with `events` in it, answering filters by kind and a few
/// tags.
#[derive(Default)]
pub(crate) struct Fake {
    pub events: Vec<BuzzEvent>,
    pub published: Mutex<Vec<Template>>,
    pub remembered: Mutex<Vec<(String, String, u64)>>,
    pub media: Mutex<Vec<String>>,
    /// The owner's channel templates, where the host has them.
    pub templates: Option<Vec<ChannelTemplate>>,
    /// Shows each channel it was asked to create.
    pub shows_created: bool,
    /// An event kind the community refuses.
    pub refuses: Option<u32>,
}

fn matches(filter: &Value, item: &BuzzEvent) -> bool {
    filter.as_object().unwrap().iter().all(|(key, value)| {
        let strings = || -> Vec<String> {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        };
        match key.as_str() {
            "kinds" => value
                .as_array()
                .unwrap()
                .iter()
                .any(|kind| kind.as_u64() == Some(u64::from(item.kind))),
            "ids" => strings().contains(&item.id),
            "authors" => strings().contains(&item.pubkey),
            key if key.starts_with('#') => {
                let name = &key[1..];
                strings().iter().any(|wanted| {
                    item.tags
                        .iter()
                        .any(|t| t[0] == name && t.get(1) == Some(wanted))
                })
            }
            _ => true,
        }
    })
}

impl BuzzClient for Fake {
    fn pubkey(&self) -> String {
        me()
    }
    fn query(&self, filters: Vec<Value>) -> BoxFuture<'static, ClientResult<Vec<BuzzEvent>>> {
        let created: Vec<BuzzEvent> = match self.shows_created {
            true => self
                .published
                .lock()
                .unwrap()
                .iter()
                .filter(|template| template.kind == 9007)
                .map(|template| metadata(&template.tags[0][1], &[]))
                .collect(),
            false => Vec::new(),
        };
        let found = self
            .events
            .iter()
            .chain(&created)
            .filter(|item| filters.iter().any(|filter| matches(filter, item)))
            .cloned()
            .collect();
        Box::pin(async move { Ok(found) })
    }
    fn publish(&self, template: Template) -> BoxFuture<'static, ClientResult<BuzzEvent>> {
        if self.refuses == Some(template.kind) {
            return Box::pin(async { Err("refused".to_string()) });
        }
        self.published.lock().unwrap().push(template.clone());
        let published = event(|event| {
            event.kind = template.kind;
            event.content = template.content;
            event.tags = template.tags;
            event.pubkey = me();
            event.id = "e".repeat(64);
        });
        Box::pin(async move { Ok(published) })
    }
    fn media(&self, url: String) -> BoxFuture<'static, ClientResult<String>> {
        use base64::Engine;
        self.media.lock().unwrap().push(url.clone());
        let bytes: &[u8] = if url.ends_with(".png") {
            &PNG
        } else {
            &[1, 2, 3]
        };
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        Box::pin(async move { Ok(data) })
    }
    fn upload(&self, _: Vec<u8>, mime: String) -> BoxFuture<'static, ClientResult<Upload>> {
        Box::pin(async move {
            Ok(Upload {
                url: "https://relay/media/x.png".into(),
                sha256: "ab".into(),
                size: 7,
                mime,
                dim: Some("1x1".into()),
                ..Default::default()
            })
        })
    }
    fn read(&self, path: String) -> BoxFuture<'static, ClientResult<Vec<u8>>> {
        Box::pin(async move {
            Ok(if path.ends_with(".png") {
                PNG.to_vec()
            } else {
                format!("# {path}").into_bytes()
            })
        })
    }
    fn memories(&self) -> BoxFuture<'static, ClientResult<Vec<Memory>>> {
        Box::pin(async {
            Ok(vec![Memory {
                slug: "mem/notes".into(),
                body: "old".into(),
                created_at: 50,
            }])
        })
    }
    fn remember(
        &self,
        slug: String,
        body: String,
        after: u64,
    ) -> BoxFuture<'static, ClientResult<()>> {
        self.remembered.lock().unwrap().push((slug, body, after));
        Box::pin(async { Ok(()) })
    }
    fn templates(&self) -> Option<BoxFuture<'static, ClientResult<Vec<ChannelTemplate>>>> {
        let templates = self.templates.clone()?;
        Some(Box::pin(async move { Ok(templates) }))
    }
}

fn community() -> Vec<BuzzEvent> {
    let profile = |pubkey: String, name: &str| {
        let content = json!({ "display_name": name }).to_string();
        event(|event| {
            event.kind = 0;
            event.pubkey = pubkey;
            event.content = content;
        })
    };
    vec![
        event(|event| {
            event.kind = 39002;
            event.tags = tags(&[
                &["d", "chan"],
                &["p", &alice(), "", "member"],
                &["p", &bob(), "", "admin"],
                &["p", &me(), "", "bot"],
            ]);
        }),
        profile(alice(), "Alice Smith"),
        profile(bob(), "Bob"),
        event(|event| event.id = root()),
        event(|event| {
            event.id = nested();
            event.tags = tags(&[&["h", "chan"], &["e", &root(), "", "reply"]]);
        }),
    ]
}

fn fake(events: Vec<BuzzEvent>) -> Arc<Fake> {
    Arc::new(Fake {
        events,
        ..Default::default()
    })
}

fn in_thread() -> Context {
    Context {
        channel: Some("chan".into()),
        root: Some(root()),
        ..Context::default()
    }
}

#[tokio::test]
async fn a_top_level_chat_refuses_replies_into_its_threads() {
    let client = fake(community());
    let chat = Context {
        channel: Some("chan".into()),
        root: None,
        top_level: true,
    };
    for value in [
        json!({ "audience": "everyone", "text": "hi", "reply": root() }),
        json!({ "audience": "everyone", "text": "hi", "reply": nested() }),
        json!({ "audience": "everyone", "text": "hi", "channel": "chan", "reply": root() }),
    ] {
        let error = call(&client, &chat, "send", value).await.unwrap_err();
        assert_eq!(
            error,
            "This chat stays at the top level: send again without reply."
        );
    }
    assert!(client.published.lock().unwrap().is_empty());
    call(
        &client,
        &chat,
        "send",
        json!({ "audience": "everyone", "text": "hi" }),
    )
    .await
    .unwrap();
    assert_eq!(
        client.published.lock().unwrap()[0].tags,
        tags(&[&["h", "chan"]])
    );
}

fn args(value: Value) -> Args {
    value.as_object().unwrap().clone()
}

async fn call(client: &Arc<Fake>, context: &Context, name: &str, value: Value) -> ToolResult {
    call_tool(client.clone(), context, name, &args(value))
        .await
        .map(|reply| reply.text())
}

#[tokio::test]
async fn replies_in_the_current_thread_and_notifies_the_members_it_names() {
    let client = fake(community());
    call(
        &client,
        &in_thread(),
        "send",
        json!({ "audience": "everyone", "text": "@Alice Smith see `@Bob`" }),
    )
    .await
    .unwrap();
    assert_eq!(
        *client.published.lock().unwrap(),
        vec![Template {
            kind: 9,
            content: "@Alice Smith see `@Bob`".into(),
            tags: tags(&[
                &["h", "chan"],
                &["e", &root(), "", "reply"],
                &["p", &alice()]
            ]),
        }]
    );
}

#[tokio::test]
async fn refuses_to_post_a_name_it_cannot_resolve_unless_told_who_it_means() {
    let client = fake(community());
    let error = call(
        &client,
        &in_thread(),
        "send",
        json!({ "audience": "everyone", "text": "hi @Carol" }),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("No single channel member is @carol"),
        "{error}"
    );
    call(
        &client,
        &in_thread(),
        "send",
        json!({ "audience": "everyone", "text": "hi @Carol", "mentions": [bob()] }),
    )
    .await
    .unwrap();
    assert!(
        client.published.lock().unwrap()[0]
            .tags
            .contains(&vec!["p".into(), bob()])
    );
}

#[tokio::test]
async fn replies_to_a_message_inside_a_thread_at_its_root_and_posts_a_files_text_and_images() {
    let client = fake(community());
    call(
        &client,
        &in_thread(),
        "send",
        json!({ "audience": "everyone", "reply": nested(), "path": "notes.md", "files": ["shot.png"] }),
    )
    .await
    .unwrap();
    assert_eq!(
        client.published.lock().unwrap()[0],
        Template {
            kind: 9,
            content: "# notes.md\n![image](https://relay/media/x.png)".into(),
            tags: tags(&[
                &["h", "chan"],
                &["e", &root(), "", "reply"],
                &[
                    "imeta",
                    "url https://relay/media/x.png",
                    "m image/png",
                    "x ab",
                    "size 7",
                    "dim 1x1",
                ],
            ]),
        }
    );
}

#[tokio::test]
async fn posts_at_the_channels_top_level_when_given_a_channel() {
    let client = fake(community());
    call(
        &client,
        &in_thread(),
        "send",
        json!({ "audience": "everyone", "channel": "other", "text": "hello" }),
    )
    .await
    .unwrap();
    assert_eq!(
        client.published.lock().unwrap()[0].tags,
        tags(&[&["h", "other"]])
    );
}

#[test]
fn the_last_root_marker_names_the_thread_as_the_relay_reads_it() {
    let (first, last, reply) = ("1".repeat(64), "2".repeat(64), "3".repeat(64));
    let message = event(|event| {
        event.tags = tags(&[
            &["e", &first, "", "root"],
            &["e", &last, "", "root"],
            &["e", &reply, "", "reply"],
        ]);
    });
    assert_eq!(root_of(&message), last);
    let lone_root = event(|event| event.tags = tags(&[&["e", &first, "", "root"]]));
    assert_eq!(root_of(&lone_root), lone_root.id);
}

#[tokio::test]
async fn reads_the_current_thread_with_each_messages_latest_edit() {
    let mut events = community();
    events.push(event(|event| {
        event.kind = 40003;
        event.created_at = 120;
        event.content = "edited".into();
        event.tags = tags(&[&["h", "chan"], &["e", &nested()]]);
    }));
    let client = fake(events);
    let text = call(&client, &in_thread(), "read", json!({}))
        .await
        .unwrap();
    assert_eq!(
        text.split('\n').collect::<Vec<_>>(),
        vec![
            format!("{} Alice Smith 1970-01-01T00:01Z: ", root()),
            format!("{} Alice Smith 1970-01-01T00:01Z: edited", nested()),
        ]
    );
}

#[test]
fn a_later_edit_by_someone_else_does_not_hide_the_authors() {
    let message = event(|event| event.created_at = 100);
    let edit = |pubkey: String, at: u64, content: &str| {
        let mut edit = message.clone();
        edit.id = format!("{at:064}");
        edit.pubkey = pubkey;
        edit.kind = 40003;
        edit.created_at = at;
        edit.content = content.into();
        edit.tags = tags(&[&["e", &message.id]]);
        edit
    };
    let shown = edited(vec![
        message.clone(),
        edit(message.pubkey.clone(), 110, "mine"),
        edit("e".repeat(64), 120, "forged"),
    ]);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].content, "mine");
}

#[tokio::test]
async fn takes_back_its_own_reaction() {
    let mut events = community();
    let reaction = event(|event| {
        event.id = "9".repeat(64);
        event.kind = 7;
        event.pubkey = me();
        event.content = "👍".into();
        event.tags = tags(&[&["e", &root()]]);
    });
    events.push(reaction.clone());
    let client = fake(events);
    call(
        &client,
        &Context::default(),
        "react",
        json!({ "id": root(), "emoji": "👍", "off": true }),
    )
    .await
    .unwrap();
    assert_eq!(
        *client.published.lock().unwrap(),
        vec![Template {
            kind: 5,
            content: String::new(),
            tags: vec![vec!["e".into(), reaction.id]],
        }]
    );
}

#[tokio::test]
async fn writes_memory_under_mem_newer_than_the_entry_it_replaces() {
    let client = fake(vec![]);
    call(
        &client,
        &Context::default(),
        "mem_set",
        json!({ "slug": "notes", "text": "new" }),
    )
    .await
    .unwrap();
    assert_eq!(
        *client.remembered.lock().unwrap(),
        vec![("mem/notes".into(), "new".into(), 50)]
    );
    assert_eq!(
        call(
            &client,
            &Context::default(),
            "mem_get",
            json!({ "slug": "notes" })
        )
        .await
        .unwrap(),
        "old"
    );
}

#[tokio::test]
async fn answers_mcp_requests_reporting_a_failed_call_as_a_tool_error() {
    let client: Arc<dyn BuzzClient> = fake(community());
    let context = Context::default();
    let init = respond(
        Some(client.clone()),
        &context,
        &json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                 "params": { "protocolVersion": "2025-11-25" } }),
    )
    .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    let list = respond(
        Some(client.clone()),
        &context,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    assert!(
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "send")
    );
    assert_eq!(
        respond(
            Some(client),
            &context,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                     "params": { "name": "send", "arguments": { "text": "hi" } } }),
        )
        .await,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "content": [{ "type": "text", "text": "channel is required here." }],
                "isError": true,
            },
        })
    );
}

#[tokio::test]
async fn replies_in_the_channel_of_the_message_it_answers() {
    let elsewhere = event(|event| {
        event.id = "3".repeat(64);
        event.tags = tags(&[&["h", "other"]]);
    });
    let mut events = community();
    events.push(elsewhere.clone());
    let client = fake(events);
    call(
        &client,
        &in_thread(),
        "send",
        json!({ "audience": "everyone", "reply": elsewhere.id, "text": "over here" }),
    )
    .await
    .unwrap();
    assert_eq!(
        client.published.lock().unwrap()[0].tags,
        tags(&[&["h", "other"], &["e", &elsewhere.id, "", "reply"]])
    );
}

#[tokio::test]
async fn refuses_a_memory_slug_the_community_would_not_list() {
    let client = fake(vec![]);
    let error = call(
        &client,
        &Context::default(),
        "mem_set",
        json!({ "slug": "My Notes", "text": "x" }),
    )
    .await
    .unwrap_err();
    assert!(error.starts_with("Bad slug"), "{error}");
}

#[tokio::test]
async fn lists_its_tools_before_it_can_call_them() {
    let context = Context::default();
    let list = respond(
        None,
        &context,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 13);
    let call = respond(
        None,
        &context,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                 "params": { "name": "read", "arguments": {} } }),
    )
    .await;
    assert_eq!(call["result"]["isError"], true);
    // Notifications and unknown methods.
    assert_eq!(
        respond(
            None,
            &context,
            &json!({ "method": "notifications/initialized" })
        )
        .await,
        json!({ "jsonrpc": "2.0", "id": 0, "result": {} })
    );
    assert_eq!(
        respond(None, &context, &json!({ "id": 3, "method": "nope" })).await["error"]["code"],
        -32601
    );
}

#[test]
fn code_never_notifies() {
    assert_eq!(strip_code("a `@b` c"), "a   c");
    assert_eq!(strip_code("x\n```\n@a\n```\n@b"), "x\n \n@b");
    // An unclosed fence is prose; its first two backticks are empty code.
    assert_eq!(strip_code("```\nopen @a"), " `\nopen @a");
    assert_eq!(strip_code("a ` b\nc ` d"), "a ` b\nc ` d");
}

#[test]
fn npub_links_and_pubkeys() {
    let key = buzz_identity::Identity::ephemeral().public_key();
    let npub = nostr::nips::nip19::ToBech32::to_bech32(&key).unwrap();
    assert_eq!(npub_links(&format!("hi nostr:{npub}!")), vec![npub.clone()]);
    assert_eq!(pubkey(&npub).unwrap(), key.to_hex());
    assert_eq!(pubkey(&"A".repeat(64)).unwrap(), "a".repeat(64));
    assert_eq!(pubkey("nope").unwrap_err(), "Not a pubkey: nope");
}

#[tokio::test]
async fn tags_a_message_for_agents_and_needs_the_audience_said() {
    let client = fake(community());
    call(
        &client,
        &in_thread(),
        "send",
        json!({ "text": "@Bob run the suite", "audience": "agents" }),
    )
    .await
    .unwrap();
    assert_eq!(
        client.published.lock().unwrap()[0].tags,
        tags(&[
            &["h", "chan"],
            &["e", &root(), "", "reply"],
            &["p", &bob()],
            &["audience", "agents"],
        ])
    );
    for refused in [
        json!({ "text": "hi" }),
        json!({ "text": "hi", "audience": "humans" }),
    ] {
        let error = call(&client, &in_thread(), "send", refused)
            .await
            .unwrap_err();
        assert_eq!(error, "audience must be everyone or agents.");
    }
    assert_eq!(client.published.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn shows_a_media_image_to_the_agent_and_nothing_else() {
    use base64::Engine;
    let client = fake(vec![]);
    let png = base64::engine::general_purpose::STANDARD.encode(&PNG[..]);
    assert_eq!(
        respond(
            Some(client.clone()),
            &Context::default(),
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                     "params": { "name": "media_get", "arguments": { "url": "abc.png" } } }),
        )
        .await,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": { "content": [{ "type": "image", "data": png, "mimeType": "image/png" }] },
        })
    );
    assert_eq!(*client.media.lock().unwrap(), ["abc.png"]);
    let error = call(
        &client,
        &Context::default(),
        "media_get",
        json!({ "url": "abc.txt" }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Only JPEG, PNG, GIF and WebP"), "{error}");
}

const CHANNEL: &str = "6f0c3a4e-6d55-4a8f-9a55-2f1d2c3b4a5e";

fn metadata(id: &str, extra: &[&[&str]]) -> BuzzEvent {
    event(|event| {
        event.kind = 39000;
        let mut list = vec![vec!["d".to_string(), id.to_string()]];
        list.extend(tags(extra));
        event.tags = list;
    })
}

fn roster(id: &str, members: &[(&str, &str)]) -> BuzzEvent {
    event(|event| {
        event.kind = 39002;
        let mut list = vec![vec!["d".to_string(), id.to_string()]];
        list.extend(members.iter().map(|(key, role)| {
            vec![
                "p".to_string(),
                key.to_string(),
                String::new(),
                role.to_string(),
            ]
        }));
        event.tags = list;
    })
}

fn in_channel() -> Context {
    Context {
        channel: Some(CHANNEL.into()),
        ..Context::default()
    }
}

#[tokio::test]
async fn lists_every_channel_it_can_see_with_all() {
    let me = me();
    let client = fake(vec![
        roster("chan", &[(&me, "member")]),
        metadata("chan", &[&["name", "general"], &["t", "stream"]]),
        metadata(
            "other",
            &[&["name", "ideas"], &["t", "forum"], &["private"]],
        ),
    ]);
    let joined = call(&client, &in_thread(), "channels", json!({}))
        .await
        .unwrap();
    assert_eq!(joined, "chan general");
    let all = call(&client, &in_thread(), "channels", json!({ "all": true }))
        .await
        .unwrap();
    assert_eq!(
        all,
        "chan general\nother ideas (forum, private, not joined)"
    );
}

#[tokio::test]
async fn shows_a_channels_details_and_refuses_what_an_action_does_not_take() {
    let me = me();
    let client = fake(vec![
        metadata(
            CHANNEL,
            &[
                &["name", "general"],
                &["t", "stream"],
                &["about", "Talk"],
                &["ttl", "3600"],
            ],
        ),
        roster(CHANNEL, &[(&me, "admin")]),
    ]);
    assert_eq!(
        call(&client, &in_channel(), "channel", json!({}))
            .await
            .unwrap(),
        format!(
            "{CHANNEL} general (stream, open, archives after 3600s idle)\nabout: Talk\nyou: admin"
        )
    );
    let refused = |value: Value| {
        let client = client.clone();
        async move {
            call(&client, &in_channel(), "channel", value)
                .await
                .unwrap_err()
        }
    };
    assert_eq!(
        refused(json!({ "action": "rename" })).await,
        "action must be one of details, create, update, join, leave, archive, unarchive, delete."
    );
    assert_eq!(
        refused(json!({ "action": "join", "name": "x" })).await,
        "name cannot be used with join."
    );
    assert_eq!(
        refused(
            json!({ "action": "create", "channel": CHANNEL, "name": "x", "visibility": "open" })
        )
        .await,
        "create makes a new channel; leave out channel."
    );
    assert_eq!(
        refused(json!({ "channel": "  " })).await,
        "channel must be a channel ID."
    );
    assert!(client.published.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn creates_a_channel_and_reports_only_what_the_community_shows() {
    let client = fake(vec![]);
    let error = call(
        &client,
        &in_thread(),
        "channel",
        json!({ "action": "create", "name": "x" }),
    )
    .await
    .unwrap_err();
    assert_eq!(error, "visibility must be open or private.");
    let error = call(
        &client,
        &in_thread(),
        "channel",
        json!({ "action": "create", "name": "x", "visibility": "open", "template": "Launch" }),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        "Channel templates are kept in Buzz Desktop and are unavailable here."
    );
    assert!(client.published.lock().unwrap().is_empty());
    let answer = call(
        &client,
        &in_thread(),
        "channel",
        json!({
            "action": "create", "name": "#launch ", "visibility": "private",
            "about": "Ship it", "ttl": 60,
        }),
    )
    .await
    .unwrap();
    let published = client.published.lock().unwrap()[0].clone();
    assert_eq!(published.kind, 9007);
    let id = published.tags[0][1].clone();
    assert_eq!(
        published.tags[1..],
        tags(&[
            &["name", "launch"],
            &["visibility", "private"],
            &["channel_type", "stream"],
            &["about", "Ship it"],
            &["ttl", "60"],
        ])
    );
    assert_eq!(
        answer,
        format!("The community accepted channel {id} but does not show it yet.")
    );
}

fn launch(agents: Vec<String>) -> ChannelTemplate {
    ChannelTemplate {
        name: "Launch".into(),
        canvas: "# Plan".into(),
        agents,
    }
}

#[tokio::test(start_paused = true)]
async fn creates_a_channel_from_its_owners_template_trying_each_agent() {
    let client = Arc::new(Fake {
        templates: Some(vec![launch(vec![alice(), me(), bob()])]),
        shows_created: true,
        ..Default::default()
    });
    let answer = call(
        &client,
        &in_thread(),
        "channel",
        json!({ "action": "create", "name": "x", "visibility": "open", "template": " launch " }),
    )
    .await
    .unwrap();
    let published = client.published.lock().unwrap().clone();
    let kinds: Vec<u32> = published.iter().map(|template| template.kind).collect();
    assert_eq!(kinds, [9007, 40100, 9000, 9000]);
    let id = published[0].tags[0][1].clone();
    assert_eq!(published[1].content, "# Plan");
    assert_eq!(
        published[1].tags,
        tags(&[&["h", &id], &["expected-revision", "none"]])
    );
    assert_eq!(
        answer,
        format!(
            "created {id}\nstarted the Canvas\n{}: the community accepted it but does not show it yet\n{}: the community accepted it but does not show it yet",
            alice(),
            bob()
        )
    );
}

#[tokio::test(start_paused = true)]
async fn adds_no_template_agents_when_it_cannot_start_the_canvas() {
    let client = Arc::new(Fake {
        templates: Some(vec![launch(vec![alice()])]),
        shows_created: true,
        refuses: Some(40100),
        ..Default::default()
    });
    let answer = call(
        &client,
        &in_thread(),
        "channel",
        json!({ "action": "create", "name": "x", "visibility": "open", "template": "Launch" }),
    )
    .await
    .unwrap();
    let published = client.published.lock().unwrap().clone();
    assert_eq!(published.len(), 1);
    let id = published[0].tags[0][1].clone();
    assert_eq!(
        answer,
        format!("created {id}\nCanvas not started, so no agents were added: refused")
    );
}

#[tokio::test(start_paused = true)]
async fn creates_nothing_from_a_template_it_cannot_name_exactly() {
    let named = |name: &str| ChannelTemplate {
        name: name.into(),
        ..ChannelTemplate::default()
    };
    let client = Arc::new(Fake {
        templates: Some(vec![named("Team"), named("team")]),
        ..Default::default()
    });
    let create = |template: &'static str| {
        let client = client.clone();
        async move {
            call(
                &client,
                &in_thread(),
                "channel",
                json!({ "action": "create", "name": "x", "visibility": "open", "template": template }),
            )
            .await
            .unwrap_err()
        }
    };
    assert_eq!(
        create("TEAM").await,
        "2 templates are named TEAM; your owner should rename one."
    );
    assert_eq!(
        create("other").await,
        "No template named other. Templates: Team, team."
    );
    assert!(client.published.lock().unwrap().is_empty());
    let none = Arc::new(Fake {
        templates: Some(Vec::new()),
        ..Default::default()
    });
    let error = call(
        &none,
        &in_thread(),
        "channel",
        json!({ "action": "create", "name": "x", "visibility": "open", "template": "Launch" }),
    )
    .await
    .unwrap_err();
    assert_eq!(error, "No template named Launch. Your owner has none.");
}

#[tokio::test]
async fn changes_only_the_name_of_a_session() {
    let about = buzz_relay_client::discovery::SESSION_CHANNEL_DESCRIPTION;
    let client = fake(vec![metadata(
        CHANNEL,
        &[&["name", "s"], &["about", about]],
    )]);
    let error = call(
        &client,
        &in_channel(),
        "channel",
        json!({ "action": "update", "topic": "x" }),
    )
    .await
    .unwrap_err();
    assert_eq!(error, "Only the name of a session can be changed.");
}

#[tokio::test(start_paused = true)]
async fn joins_and_confirms_from_the_roster() {
    let me = me();
    let client = fake(vec![roster(CHANNEL, &[(&me, "member")])]);
    let answer = call(
        &client,
        &in_channel(),
        "channel",
        json!({ "action": "join" }),
    )
    .await
    .unwrap();
    assert_eq!(answer, format!("joined {CHANNEL}"));
    assert_eq!(
        client.published.lock().unwrap()[0],
        Template {
            kind: 9021,
            content: String::new(),
            tags: tags(&[&["h", CHANNEL]]),
        }
    );
    let answer = call(
        &client,
        &in_channel(),
        "channel",
        json!({ "action": "leave" }),
    )
    .await
    .unwrap();
    assert_eq!(
        answer,
        "The community accepted leave but does not show it yet."
    );
}

#[tokio::test(start_paused = true)]
async fn adds_and_removes_members_one_at_a_time() {
    let mut events = community();
    events.push(roster(CHANNEL, &[(&alice(), "member")]));
    let client = fake(events);
    let refused = |value: Value| {
        let client = client.clone();
        async move {
            call(&client, &in_channel(), "members", value)
                .await
                .unwrap_err()
        }
    };
    assert_eq!(
        refused(json!({ "add": [me()] })).await,
        "For yourself, use channel with action join or leave."
    );
    assert_eq!(
        refused(json!({ "add": [bob()], "remove": [bob()] })).await,
        "Add or remove someone, not both."
    );
    assert_eq!(
        refused(json!({ "role": "admin" })).await,
        "role is for the members you add."
    );
    assert_eq!(
        refused(json!({ "add": [bob()], "role": "king" })).await,
        "role must be one of owner, admin, member, guest, bot."
    );
    assert_eq!(
        refused(json!({ "add": "x" })).await,
        "add must be a list of pubkeys."
    );
    let many: Vec<String> = (0..51).map(|n| format!("{n:064x}")).collect();
    assert_eq!(
        refused(json!({ "add": many })).await,
        "Change at most 50 members at a time."
    );
    assert!(client.published.lock().unwrap().is_empty());
    let answer = call(
        &client,
        &in_channel(),
        "members",
        json!({ "add": [bob(), bob()], "remove": [alice()], "role": "guest" }),
    )
    .await
    .unwrap();
    assert_eq!(
        *client.published.lock().unwrap(),
        vec![
            Template {
                kind: 9000,
                content: String::new(),
                tags: tags(&[&["h", CHANNEL], &["p", &bob()], &["role", "guest"]]),
            },
            Template {
                kind: 9001,
                content: String::new(),
                tags: tags(&[&["h", CHANNEL], &["p", &alice()]]),
            },
        ]
    );
    assert_eq!(
        answer,
        format!(
            "{} Bob: the community accepted it but does not show it yet\n{} Alice Smith: the community accepted it but does not show it yet",
            bob(),
            alice()
        )
    );
}
