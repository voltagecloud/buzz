//! buzz-app's `transcript.test.ts`.

use super::*;
use crate::agent_plugins::codex::rpc::testing::FakeCodex;
use core::prelude::v1::test;
use std::collections::HashMap;
use std::sync::Arc;

const ROOT: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn place() -> Place {
    Place {
        channel_id: "channel".into(),
        root: Some(ROOT.into()),
    }
}

const INPUT: &str = "<context>\n{\"channel_id\":\"channel\"}\n</context>\n<buzz-event>\n{\"id\":\"e\",\"pubkey\":\"sam\"}\nRequest: \"fix the \\u003ctests\\u003e\"\n</buzz-event>";

fn turn(patch: Value) -> Value {
    let mut turn = json!({
        "id": "turn-1",
        "status": "completed",
        "error": null,
        "startedAt": 100,
        "completedAt": 160,
        "items": [
            { "type": "userMessage", "id": "u1", "content": [{ "type": "text", "text": INPUT }] },
            { "type": "reasoning", "id": "r1", "summary": ["Looking at tests"] },
            {
                "type": "commandExecution",
                "id": "c1",
                "command": "/bin/zsh -lc 'pnpm test'",
                "aggregatedOutput": "1 failed",
                "status": "failed",
                "durationMs": 2000,
            },
            {
                "type": "userMessage",
                "id": "u2",
                "content": [{
                    "type": "text",
                    "text": "<new-message-arrived-while-you-were-working>\n{}\nRequest: \"also lint\"\n</new-message-arrived-while-you-were-working>",
                }],
            },
            {
                "type": "fileChange",
                "id": "f1",
                "status": "completed",
                "changes": [{ "path": "a.ts", "kind": "update", "diff": "-a\n+b" }],
            },
            {
                "type": "dynamicToolCall",
                "id": "d1",
                "namespace": "buzz",
                "tool": "send",
                "arguments": { "text": "Done" },
                "status": "completed",
                "contentItems": [{ "type": "inputText", "text": "sent" }],
                "success": true,
            },
            { "type": "agentMessage", "id": "a1", "text": "Fixed." },
            { "type": "contextCompaction", "id": "x1" },
            // A final answer after the reply went through send.
            { "type": "agentMessage", "id": "a2", "text": "" },
        ],
    });
    if let (Value::Object(turn), Value::Object(patch)) = (&mut turn, patch) {
        turn.extend(patch);
    }
    turn
}

fn nobody(_: &str) -> Option<String> {
    None
}

fn tool(item: &Item) -> &ToolItem {
    match &item.kind {
        ItemKind::Tool(tool) => tool,
        other => panic!("not a tool: {other:?}"),
    }
}

#[test]
fn shows_a_codex_turns_request_steering_tools_and_reply() {
    let sam = |pubkey: &str| (pubkey == "sam").then(|| "Sam".to_string());
    let mapped = &codex_turns(&[turn(json!({}))], &place(), &sam)[0];
    assert_eq!(mapped.turn_id, "turn-1");
    assert_eq!(mapped.channel_id.as_deref(), Some("channel"));
    assert_eq!(mapped.thread_root, Some(Some(ROOT.into())));
    assert_eq!(
        (mapped.started_at, mapped.ended_at),
        (100_000, Some(160_000))
    );
    let items = &mapped.items;
    assert_eq!(items.len(), 7);
    match &items[0].kind {
        ItemKind::Prompt {
            text,
            author,
            sections,
            steer,
        } => {
            assert_eq!(text, "fix the <tests>");
            assert_eq!(author.as_deref(), Some("Sam"));
            assert!(!steer);
            let tags: Vec<_> = sections.iter().map(|s| s.tag.as_str()).collect();
            assert_eq!(tags, ["context", "buzz-event"]);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(&items[1].kind, ItemKind::Thought { text, .. } if text == "Looking at tests"));
    let command = tool(&items[2]);
    assert_eq!(
        (
            command.title.as_str(),
            command.kind.as_str(),
            command.status.as_str(),
            command.output.as_str()
        ),
        ("pnpm test", "execute", "failed", "1 failed")
    );
    assert_eq!(command.completed_at, Some(102_000));
    assert!(
        matches!(&items[3].kind, ItemKind::Prompt { text, steer: true, .. } if text == "also lint")
    );
    let edit = tool(&items[4]);
    assert_eq!(edit.kind, "edit");
    assert_eq!(edit.paths, ["a.ts"]);
    assert_eq!(edit.output, "-a\n+b");
    assert!(matches!(
        &items[5].kind,
        ItemKind::Message { text, posted: true, .. } if text == "Done"
    ));
    assert!(matches!(
        &items[6].kind,
        ItemKind::Message { text, posted: false, .. } if text == "Fixed."
    ));
    let channel = &codex_turns(
        &[turn(
            json!({ "status": "interrupted", "completedAt": null, "items": [] }),
        )],
        &Place {
            channel_id: "channel".into(),
            root: None,
        },
        &nobody,
    )[0];
    assert_eq!(channel.thread_root, Some(None));
    assert_eq!(channel.stop_reason.as_deref(), Some("interrupted"));
    assert_eq!(channel.ended_at, None);
}

#[test]
fn shows_the_command_the_model_wrote_not_the_login_shell_running_it() {
    assert_eq!(
        shell_command("/bin/zsh -lc 'git diff --stat'"),
        "git diff --stat"
    );
    assert_eq!(
        shell_command(r#"/bin/bash -lc 'echo '"'"'hi'"'"''"#),
        "echo 'hi'"
    );
    assert_eq!(
        shell_command(r#"/bin/zsh -lc "sed -n '1,9p' a | grep \"\$x\"""#),
        r#"sed -n '1,9p' a | grep "$x""#
    );
    assert_eq!(shell_command("pnpm test"), "pnpm test");
}

#[test]
fn keeps_a_send_that_failed_or_has_not_finished_as_a_tool_call() {
    let send = |id: &str, patch: Value| {
        let mut send = json!({
            "type": "dynamicToolCall",
            "id": id,
            "namespace": "buzz",
            "tool": "send",
            "arguments": { "text": "Done" },
        });
        if let (Value::Object(send), Value::Object(patch)) = (&mut send, patch) {
            send.extend(patch);
        }
        send
    };
    let mapped = &codex_turns(
        &[turn(json!({ "items": [
            send("d1", json!({
                "status": "completed",
                "contentItems": [{ "type": "inputText", "text": "channel not found" }],
                "success": false,
            })),
            send("d2", json!({ "status": "inProgress", "contentItems": null, "success": null })),
        ] }))],
        &place(),
        &nobody,
    )[0];
    let failed = tool(&mapped.items[0]);
    assert_eq!(
        (
            failed.title.as_str(),
            failed.status.as_str(),
            failed.output.as_str()
        ),
        ("buzz.send", "failed", "channel not found")
    );
    let pending = tool(&mapped.items[1]);
    assert_eq!(
        (pending.title.as_str(), pending.status.as_str()),
        ("buzz.send", "in_progress")
    );
}

fn lookup() -> Lookup {
    Lookup {
        agent_pubkey: "a".repeat(64),
        agent_name: "Sol".into(),
        channel_id: "channel".into(),
        name: Some("dev".into()),
        root: Some(ROOT.into()),
        thread_id: None,
    }
}

fn saved() -> Lookup {
    Lookup {
        thread_id: Some("thread-1".into()),
        ..lookup()
    }
}

fn answer(
    fake: &FakeCodex,
    method: &str,
    answer: impl Fn(&Value) -> Result<Value, String> + Send + Sync + 'static,
) {
    fake.state().answers.insert(method.into(), Arc::new(answer));
}

/// `thread/turns/list` over a thread's turns, newest first, a page at a
/// time.
fn paged(newest: Vec<Value>) -> impl Fn(&Value) -> Result<Value, String> + Send + Sync {
    move |params| {
        let start: usize = params["cursor"].as_str().map_or(0, |c| c.parse().unwrap());
        let end = (start + params["limit"].as_u64().unwrap() as usize).min(newest.len());
        Ok(json!({
            "data": newest[start.min(end)..end],
            "nextCursor": (end < newest.len()).then(|| end.to_string()),
        }))
    }
}

/// `thread/list` as Codex searches names.
fn threads(fake: &FakeCodex, threads: Vec<(&'static str, String, u64)>) {
    answer(fake, "thread/list", move |params| {
        let term = params["searchTerm"].as_str().unwrap_or("");
        Ok(json!({ "data": threads
            .iter()
            .filter(|(_, name, _)| name.contains(term))
            .map(|(id, name, updated)| json!({ "id": id, "name": name, "updatedAt": updated }))
            .collect::<Vec<_>>() }))
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_the_latest_turns_oldest_first_a_turn_per_request_and_closes_its_app_server() {
    let fake = FakeCodex::default();
    let mut newest = vec![
        turn(json!({ "id": "turn-2", "status": "inProgress", "completedAt": null })),
        turn(json!({})),
    ];
    newest.extend(
        (0..TURN_LIMIT).map(|index| {
            turn(json!({ "id": format!("earlier-{index}"), "startedAt": 99 - index }))
        }),
    );
    answer(&fake, "thread/turns/list", paged(newest));
    threads(&fake, Vec::new());
    let read = read_transcript(&fake.spawner(), &saved(), &Signal::default(), &nobody)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fake.spawned("app-server").len(), 1);
    let requests = fake.calls("thread/turns/list");
    assert_eq!(requests.len(), TURN_LIMIT);
    assert_eq!(
        requests[0]["params"],
        json!({ "threadId": "thread-1", "limit": 1, "sortDirection": "desc", "itemsView": "full" })
    );
    assert_eq!(
        requests[1]["params"],
        json!({ "threadId": "thread-1", "limit": 1, "sortDirection": "desc", "itemsView": "full", "cursor": "1" })
    );
    let ids: Vec<_> = read
        .transcript
        .turns
        .iter()
        .map(|turn| turn.turn_id.as_str())
        .collect();
    assert_eq!(ids.len(), TURN_LIMIT);
    assert_eq!(ids[TURN_LIMIT - 2..], ["turn-1", "turn-2"]);
    assert!(read.more && read.working);
    assert_eq!(fake.state().ended, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_turns_that_each_fit_app_servers_output_frame_but_together_do_not() {
    // Three turns of 1.5 MiB: one per frame fits the 4 MiB bound, all
    // three do not.
    let big = |id: &str| {
        turn(json!({ "id": id, "items": [{
            "type": "commandExecution",
            "id": "c",
            "command": "cat log",
            "aggregatedOutput": "x".repeat(1536 * 1024),
            "status": "completed",
        }] }))
    };
    let fake = FakeCodex::default();
    answer(
        &fake,
        "thread/turns/list",
        paged(vec![big("c"), big("b"), big("a")]),
    );
    threads(&fake, Vec::new());
    let read = read_transcript(&fake.spawner(), &saved(), &Signal::default(), &nobody)
        .await
        .unwrap()
        .unwrap();
    let ids: Vec<_> = read
        .transcript
        .turns
        .iter()
        .map(|turn| turn.turn_id.as_str())
        .collect();
    assert_eq!(ids, ["a", "b", "c"]);
    assert!(!read.more);
}

#[tokio::test(flavor = "multi_thread")]
async fn merges_the_threads_other_copies_started_found_by_tag_or_by_their_earlier_name() {
    let tag = super::super::prompt::session_tag(&"a".repeat(64), "channel", Some(ROOT));
    let title = format!("Buzz #dev · thread {} · Sol", &ROOT[..8]);
    let named = vec![
        ("tagged", format!("Renamed · {tag}"), 3),
        ("legacy", title.clone(), 2),
        // Same name, from a same-named channel elsewhere.
        ("same-name", title.clone(), 9),
        ("other-agent", format!("{title} · buzz:0000000000000000"), 4),
    ];
    let started: HashMap<&str, (u64, &str)> = HashMap::from([
        ("thread-1", (300, "channel")),
        ("tagged", (200, "channel")),
        ("legacy", (100, "channel")),
        ("same-name", (400, "elsewhere")),
    ]);
    let list = move |params: &Value| {
        let thread = params["threadId"].as_str().unwrap_or("").to_string();
        let (at, channel) = started.get(thread.as_str()).copied().unwrap_or((0, ""));
        let text = format!(
            "<context>\n{}\n</context>\nRequest: \"hi\"",
            json!({ "channel_id": channel, "session_thread": ROOT })
        );
        Ok(json!({
            "data": [turn(json!({
                "id": thread,
                "startedAt": at,
                "items": [{ "type": "userMessage", "id": "u", "content": [{ "type": "text", "text": text }] }],
            }))],
            "nextCursor": null,
        }))
    };
    let server = |found: Vec<(&'static str, String, u64)>| {
        let fake = FakeCodex::default();
        answer(&fake, "thread/turns/list", list.clone());
        threads(&fake, found);
        fake
    };
    let fake = server(named.clone());
    let read = read_transcript(&fake.spawner(), &saved(), &Signal::default(), &nobody)
        .await
        .unwrap()
        .unwrap();
    let ids: Vec<_> = read
        .transcript
        .turns
        .iter()
        .map(|turn| turn.turn_id.as_str())
        .collect();
    assert_eq!(ids, ["legacy", "tagged", "thread-1"]);
    let searches: Vec<_> = fake
        .calls("thread/list")
        .iter()
        .map(|call| call["params"].clone())
        .collect();
    assert_eq!(
        searches,
        [
            json!({ "searchTerm": tag, "limit": 50 }),
            json!({ "searchTerm": title, "limit": 50 }),
        ]
    );
    // With no saved thread, the newest one found for it is the
    // conversation's.
    let signal = Signal::default();
    assert_eq!(
        find_conversation(&server(named.clone()).spawner(), &lookup(), &signal).await,
        Ok(Some(("tagged".into(), 3000)))
    );
    assert_eq!(
        find_conversation(&server(named[1..].to_vec()).spawner(), &lookup(), &signal).await,
        Ok(Some(("legacy".into(), 2000)))
    );
    let channel = Lookup {
        root: None,
        ..lookup()
    };
    assert_eq!(
        find_conversation(&server(Vec::new()).spawner(), &channel, &signal).await,
        Ok(None)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn treats_a_thread_codex_no_longer_has_as_missing_and_other_failures_as_errors() {
    let gone = FakeCodex::default();
    answer(&gone, "thread/turns/list", |_| {
        Err("thread not loaded: thread-1".into())
    });
    threads(&gone, Vec::new());
    assert_eq!(
        read_transcript(&gone.spawner(), &saved(), &Signal::default(), &nobody).await,
        Ok(None)
    );
    let broken = FakeCodex::default();
    answer(&broken, "thread/turns/list", |_| {
        Err("database is locked".into())
    });
    threads(&broken, Vec::new());
    assert_eq!(
        read_transcript(&broken.spawner(), &saved(), &Signal::default(), &nobody).await,
        Err("database is locked".into())
    );
}
