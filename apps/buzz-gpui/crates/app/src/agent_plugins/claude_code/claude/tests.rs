//! buzz-app's `claude.test.ts`: recorded stream-json lines in, settled
//! turns and control responses out.

use super::testing::{FakeSpawn, flush, until};
use super::*;
use crate::agent_plugins::process::{ProcessControl, Spawn, Spawned};
use std::sync::Mutex as StdMutex;

fn launch() -> Launch {
    Launch {
        cwd: "~/.buzz".into(),
        system_prompt: "<base>be useful</base>".into(),
        session_id: Some("s1".into()),
        ..Default::default()
    }
}

async fn start(fake: &FakeSpawn, launch: Launch) -> ClaudeProcess {
    ClaudeProcess::start(&fake.spawner(), launch).await.unwrap()
}

#[tokio::test]
async fn starts_claude_code_as_the_sdk_does_and_initializes_it() {
    let fake = FakeSpawn::default();
    let claude = start(
        &fake,
        Launch {
            model: Some("opus".into()),
            session_id: Some("new-session".into()),
            ..launch()
        },
    )
    .await;
    let process = &fake.claudes()[0];
    assert_eq!(
        process.args(),
        [
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
            "--replay-user-messages",
            "--permission-mode",
            "bypassPermissions",
            "--model",
            "opus",
            "--session-id",
            "new-session",
        ]
    );
    let state = process.state();
    assert_eq!(state.options.cwd.as_deref(), Some("~/.buzz"));
    assert_eq!(state.options.env["CLAUDECODE"], None);
    assert_eq!(state.options.env["CLAUDE_CODE_SESSION_ID"], None);
    assert_eq!(state.options.env["ANTHROPIC_API_KEY"], None);
    assert_eq!(
        state.options.env["CLAUDE_CODE_ENTRYPOINT"].as_deref(),
        Some("sdk-ts")
    );
    assert_eq!(state.received[0]["type"], "control_request");
    assert_eq!(state.received[0]["request"]["subtype"], "initialize");
    assert_eq!(
        state.received[0]["request"]["appendSystemPrompt"],
        "<base>be useful</base>"
    );
    drop(state);
    assert_eq!(claude.session_id().as_deref(), Some("new-session"));
    assert!(!claude.started());
}

#[tokio::test]
async fn resolves_a_message_once_idle_and_keeps_the_process() {
    let fake = FakeSpawn::default();
    let claude = start(&fake, launch()).await;
    assert_eq!(
        claude.send("hello", TurnPick::default()).await,
        Settled::ok()
    );
    assert!(claude.started());
    assert!(!claude.busy());
    assert_eq!(
        claude.send("again", TurnPick::default()).await,
        Settled::ok()
    );
    assert_eq!(fake.claudes().len(), 1);
    assert_eq!(fake.claudes()[0].prompts(), ["hello", "again"]);
}

#[tokio::test]
async fn answers_both_messages_of_a_turn_that_took_a_second_mid_way() {
    let fake = FakeSpawn::default();
    let claude = start(&fake, launch()).await;
    let process = fake.claudes()[0].clone();
    process.set_hold(true);
    let first = tokio::spawn(claude.send("first", TurnPick::default()));
    let second = tokio::spawn(claude.send("second", TurnPick::default()));
    assert!(claude.busy());
    flush().await;
    assert!(!first.is_finished());
    process.finish(None);
    assert_eq!(first.await.unwrap(), Settled::ok());
    assert_eq!(second.await.unwrap(), Settled::ok());
}

/// Spawns as `inner` does, but the fake does not echo messages by itself,
/// so a test decides when Claude takes each one.
struct ManualReplay(FakeSpawn);

impl Spawn for ManualReplay {
    fn spawn(
        &self,
        id: &str,
        mut options: SpawnOptions,
    ) -> BoxFuture<'static, Result<Spawned, String>> {
        options.args.retain(|arg| arg != "--replay-user-messages");
        self.0.spawn(id, options)
    }
}

fn manual_replay(fake: &FakeSpawn) -> Spawner {
    Arc::new(ManualReplay(fake.clone()))
}

fn replay(text: &str) -> Value {
    json!({
        "type": "user", "isReplay": true, "session_id": "s1",
        "message": { "role": "user", "content": text },
    })
}

fn state(state: &str) -> Value {
    json!({
        "type": "system", "subtype": "session_state_changed",
        "state": state, "session_id": "s1",
    })
}

/// As recorded from Claude 2.1: a message written just as a turn
/// ends is followed by idle, then running, and only then taken.
#[tokio::test]
async fn a_message_written_as_the_turn_ends_waits_for_its_own_turn() {
    let fake = FakeSpawn::default();
    let claude = ClaudeProcess::start(&manual_replay(&fake), launch())
        .await
        .unwrap();
    let process = fake.claudes()[0].clone();
    process.set_hold(true);
    let first = tokio::spawn(claude.send("first", TurnPick::default()));
    flush().await;
    process.emit(replay("first"));
    let second = tokio::spawn(claude.send("second", TurnPick::default()));
    flush().await;
    process.finish(None);
    flush().await;
    assert!(
        !first.is_finished(),
        "settled before Claude read the second"
    );
    assert!(claude.busy());
    process.emit(state("running"));
    process.emit(replay("second"));
    process.emit(json!({ "type": "result", "subtype": "success", "is_error": false }));
    process.emit(state("idle"));
    assert_eq!(first.await.unwrap(), Settled::ok());
    assert_eq!(second.await.unwrap(), Settled::ok());
    assert!(!claude.busy());
}

#[tokio::test]
async fn a_message_claude_never_takes_is_settled_after_a_grace() {
    let fake = FakeSpawn::default();
    let claude = ClaudeProcess::start(&manual_replay(&fake), launch())
        .await
        .unwrap();
    let sent = tokio::spawn(claude.send("lost", TurnPick::default()));
    flush().await;
    assert!(!sent.is_finished());
    tokio::time::sleep(UNREAD_GRACE * 2).await;
    assert_eq!(sent.await.unwrap(), Settled::ok());
    assert!(!claude.busy());
}

/// Spawns as `inner` does, without the environment that asks for idle
/// reports.
struct NoIdle(FakeSpawn);

impl Spawn for NoIdle {
    fn spawn(
        &self,
        id: &str,
        mut options: SpawnOptions,
    ) -> BoxFuture<'static, Result<Spawned, String>> {
        options.env.clear();
        self.0.spawn(id, options)
    }
}

#[tokio::test]
async fn settles_on_each_result_when_claude_code_does_not_report_idle() {
    let fake = FakeSpawn::default();
    let spawner: Spawner = Arc::new(NoIdle(fake.clone()));
    let claude = ClaudeProcess::start(&spawner, launch()).await.unwrap();
    assert!(fake.claudes()[0].state().options.env.is_empty());
    assert_eq!(
        claude.send("hello", TurnPick::default()).await,
        Settled::ok()
    );
    assert!(!claude.busy());
}

#[tokio::test]
async fn reports_a_turn_that_ends_in_an_error_with_claudes_own_words() {
    let fake = FakeSpawn::default();
    let claude = start(&fake, launch()).await;
    let process = fake.claudes()[0].clone();
    process.set_hold(true);
    let sent = tokio::spawn(claude.send("hello", TurnPick::default()));
    flush().await;
    process.finish(Some("Credit balance is too low"));
    assert_eq!(
        sent.await.unwrap(),
        Settled::failed("Credit balance is too low")
    );
}

#[tokio::test]
async fn fails_what_is_waiting_when_the_process_exits() {
    let fake = FakeSpawn::default();
    let claude = start(
        &fake,
        Launch {
            resume: Some("gone".into()),
            session_id: None,
            ..launch()
        },
    )
    .await;
    assert_eq!(claude.session_id().as_deref(), Some("gone"));
    assert_eq!(
        claude.send("hello", TurnPick::default()).await,
        Settled::failed("No conversation found with session ID: gone")
    );
    assert!(!claude.running());
    assert!(!claude.started());
    assert!(!claude.send("more", TurnPick::default()).await.ok);
    assert_eq!(claude.exited().await, Some(1));
}

#[tokio::test]
async fn allows_tool_use_and_acknowledges_hooks_it_is_asked_about() {
    let fake = FakeSpawn::default();
    let _claude = start(&fake, launch()).await;
    let process = fake.claudes()[0].clone();
    process.emit(json!({
        "type": "control_request", "request_id": "r1",
        "request": { "subtype": "can_use_tool", "tool_name": "Bash", "input": { "command": "ls" } },
    }));
    process.emit(json!({
        "type": "control_request", "request_id": "r2",
        "request": { "subtype": "hook_callback", "callback_id": "h" },
    }));
    process.emit(json!({
        "type": "control_request", "request_id": "r3",
        "request": { "subtype": "mcp_message" },
    }));
    flush().await;
    assert_eq!(
        process.responses(),
        vec![
            json!({ "subtype": "success", "request_id": "r1",
                    "response": { "behavior": "allow", "updatedInput": { "command": "ls" } } }),
            json!({ "subtype": "success", "request_id": "r2", "response": {} }),
            json!({ "subtype": "error", "request_id": "r3",
                    "error": "Buzz does not handle mcp_message" }),
        ]
    );
}

#[tokio::test]
async fn serves_the_buzz_tools_and_ends_a_turn_on_a_final_send() {
    let fake = FakeSpawn::default();
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let recorded = calls.clone();
    // The tool answers when the test lets it, after the hooks, as a relay
    // round trip would. A fixed delay raced the test's own fixed waits on
    // a busy machine.
    let answer = Arc::new(tokio::sync::Notify::new());
    let release = answer.clone();
    let tools: ToolServer = Arc::new(move |conversation, message: Value| {
        recorded
            .lock()
            .unwrap()
            .push((conversation, message.clone()));
        let release = release.clone();
        Box::pin(async move {
            release.notified().await;
            Ok(json!({
                "jsonrpc": "2.0", "id": message["id"],
                "result": { "content": [{ "type": "text", "text": "sent" }] },
            }))
        })
    });
    let claude = start(
        &fake,
        Launch {
            tools: Some(tools),
            ..launch()
        },
    )
    .await;
    claude.set_conversation("chan/root");
    let process = fake.claudes()[0].clone();
    let args = process.args();
    let config = &args[args.iter().position(|arg| arg == "--mcp-config").unwrap() + 1];
    assert_eq!(
        serde_json::from_str::<Value>(config).unwrap(),
        json!({ "mcpServers": { "buzz": { "type": "sdk", "name": "buzz", "alwaysLoad": true } } })
    );
    let initialize = process.state().received[0]["request"].clone();
    assert_eq!(initialize["sdkMcpServers"], json!(["buzz"]));
    assert_eq!(
        initialize["hooks"],
        json!({ "PostToolUse": [{ "matcher": "mcp__buzz__send", "hookCallbackIds": ["final"] }] })
    );
    process.set_hold(true);
    let _turn = tokio::spawn(claude.send("hi", TurnPick::default()));
    flush().await;
    let call = json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call" });
    process.emit(json!({
        "type": "control_request", "request_id": "r1",
        "request": { "subtype": "mcp_message", "server_name": "buzz", "message": call },
    }));
    let hook = |last: bool| {
        json!({
            "type": "control_request", "request_id": format!("final-{last}"),
            "request": {
                "subtype": "hook_callback", "callback_id": "final",
                "input": {
                    "tool_name": "mcp__buzz__send",
                    "tool_input": { "text": "done", "final": last },
                    "tool_response": [{ "type": "text", "text": "sent" }],
                },
            },
        })
    };
    process.emit(hook(false));
    process.emit(hook(true));
    until(|| process.responses().len() == 2).await;
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(Some("chan/root".to_string()), call)]
    );
    let hooks = [
        json!({ "subtype": "success", "request_id": "final-false", "response": {} }),
        json!({ "subtype": "success", "request_id": "final-true",
                "response": { "continue": false } }),
    ];
    assert_eq!(process.responses(), hooks);
    answer.notify_one();
    until(|| process.responses().len() == 3).await;
    assert_eq!(
        process.responses(),
        [
            hooks[0].clone(),
            hooks[1].clone(),
            json!({ "subtype": "success", "request_id": "r1", "response": { "mcp_response": {
                "jsonrpc": "2.0", "id": 3,
                "result": { "content": [{ "type": "text", "text": "sent" }] },
            } } }),
        ]
    );
}

/// A process whose output the test writes by hand.
struct Silent;

impl ProcessControl for Silent {
    fn write(&self, _: String, _: bool) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn kill(&self) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn reads_messages_split_across_output_chunks() {
    let (output, receiver) = mpsc::unbounded_channel();
    let claude = ClaudeProcess::attach(
        Process(Arc::new(Silent)),
        receiver,
        Some("s1".into()),
        None,
        None,
    );
    let line = json!({ "type": "system", "subtype": "init", "session_id": "s2" }).to_string();
    output
        .send(ProcessOutput::Stdout(line[..10].to_string()))
        .unwrap();
    flush().await;
    assert!(!claude.started());
    output
        .send(ProcessOutput::Stdout(format!("{}\n", &line[10..])))
        .unwrap();
    flush().await;
    assert!(claude.started());
    assert_eq!(claude.session_id().as_deref(), Some("s2"));
}

fn pick(model: Option<&str>, effort: Option<&str>) -> TurnPick {
    TurnPick {
        model: model.map(str::to_string),
        effort: effort.map(str::to_string),
    }
}

#[tokio::test]
async fn runs_a_turn_at_the_effort_asked_for_and_the_next_without_one_at_the_sessions_own() {
    let fake = FakeSpawn::default();
    let claude = start(&fake, launch()).await;
    let process = fake.claudes()[0].clone();
    claude.send("plain", TurnPick::default()).await;
    let types: Vec<Value> = process
        .state()
        .received
        .iter()
        .map(|message| message["type"].clone())
        .collect();
    assert_eq!(types, ["control_request", "user"]);
    claude.send("hard", pick(None, Some("xhigh"))).await;
    process.set_hold(true);
    let quick = tokio::spawn(claude.send("quick", pick(None, Some("low"))));
    // Folded into the running turn, a message without an effort keeps it.
    let steer = tokio::spawn(claude.send("also", TurnPick::default()));
    flush().await;
    process.finish(None);
    quick.await.unwrap();
    steer.await.unwrap();
    process.set_hold(false);
    claude.send("after", TurnPick::default()).await;
    assert_eq!(
        process.prompts(),
        ["plain", "hard", "quick", "also", "after"]
    );
    assert_eq!(process.efforts(), ["high", "xhigh", "low", "low", "high"]);
    // The session's own level is read once, before the first change.
    assert_eq!(process.requests("get_settings").len(), 1);
}

#[tokio::test]
async fn runs_a_turn_on_the_model_asked_for_and_the_next_without_one_on_the_sessions_own() {
    let fake = FakeSpawn::default();
    let claude = start(
        &fake,
        Launch {
            model: Some("opus".into()),
            ..launch()
        },
    )
    .await;
    let process = fake.claudes()[0].clone();
    assert!(
        claude
            .models()
            .await
            .iter()
            .any(|model| model.value == "haiku")
    );
    claude.send("plain", TurnPick::default()).await;
    process.set_hold(true);
    let quick = tokio::spawn(claude.send("quick", pick(Some("haiku"), None)));
    // Folded into the running turn, a message without a model keeps it.
    let steer = tokio::spawn(claude.send("also", TurnPick::default()));
    flush().await;
    process.finish(None);
    quick.await.unwrap();
    steer.await.unwrap();
    process.set_hold(false);
    claude.send("after", TurnPick::default()).await;
    assert_eq!(process.models(), ["opus", "haiku", "haiku", "opus"]);
    // The session reports its own model, not the picked one.
    assert_eq!(
        claude.read_model().await.as_deref(),
        Some("claude-opus-5-5")
    );
    let set: Vec<Value> = process
        .requests("set_model")
        .iter()
        .map(|request| request["model"].clone())
        .collect();
    assert_eq!(set, [json!("haiku"), json!("opus")]);
}

fn receipt(
    store: &crate::agent_activity::local::LocalActivity,
    message: &str,
    scope: &str,
) -> Receipt {
    Receipt {
        scope: scope.into(),
        work: store.begin(
            crate::agent_activity::local::Begin {
                scope: "s".into(),
                agent: "a".into(),
                channel_id: "c".into(),
                message_id: message.into(),
                thread_root_id: scope.into(),
            },
            0,
        ),
    }
}

#[tokio::test]
async fn moves_activity_to_the_echoed_follow_up_in_its_thread_only() {
    use crate::agent_activity::local::{LocalActivity, LocalWorkState};
    let store = LocalActivity::default();
    let fake = FakeSpawn::default();
    let claude = start(&fake, launch()).await;
    let process = fake.claudes()[0].clone();
    process.set_hold(true);
    let first = tokio::spawn(claude.send_with(
        "first",
        TurnPick::default(),
        Some(receipt(&store, "m1", "root")),
    ));
    flush().await;
    let state = |n: usize| store.work()[n].state;
    assert_eq!(state(0), LocalWorkState::Working);
    // The fake answers each message with text: Claude is writing.
    assert_eq!(
        store.work()[0].step.as_ref().map(|step| step.tool.as_str()),
        Some("Writing reply")
    );
    let other = tokio::spawn(claude.send_with(
        "elsewhere",
        TurnPick::default(),
        Some(receipt(&store, "m2", "other")),
    ));
    let follow_up = tokio::spawn(claude.send_with(
        "follow up",
        TurnPick::default(),
        Some(receipt(&store, "m3", "root")),
    ));
    flush().await;
    assert_eq!(state(0), LocalWorkState::Superseded);
    assert_eq!(state(1), LocalWorkState::Working);
    assert_eq!(state(2), LocalWorkState::Working);
    process.set_hold(false);
    process.finish(None);
    for sent in [first, other, follow_up] {
        assert!(sent.await.unwrap().ok);
    }
}

#[test]
fn names_the_last_block_as_a_step() {
    let message = |block: Value| json!({ "type": "assistant", "message": { "content": [block] } });
    assert_eq!(
        step_label(&message(json!({
            "type": "tool_use", "name": "mcp__buzz__send", "input": { "text": "hi" },
        }))),
        Some(("send".into(), None))
    );
    assert_eq!(
        step_label(&message(json!({
            "type": "tool_use", "name": "Read", "input": { "file_path": "/home/me/notes.md" },
        }))),
        Some(("Read".into(), Some("notes.md".into())))
    );
    assert_eq!(
        step_label(&message(json!({
            "type": "tool_use", "name": "Bash",
            "input": { "description": "List files", "command": "ls" },
        }))),
        Some(("Bash".into(), Some("List files".into())))
    );
    assert_eq!(
        step_label(&message(json!({ "type": "thinking" }))),
        Some(("Thinking".into(), None))
    );
    assert_eq!(step_label(&message(json!({ "type": "image" }))), None);
}
