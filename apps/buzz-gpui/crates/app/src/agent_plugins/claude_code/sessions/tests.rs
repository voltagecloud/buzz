//! buzz-app's `sessions.test.ts`.

use super::*;
use crate::agent_plugins::claude_code::claude::testing::{FakeClaude, FakeSpawn, flush, until};
use crate::agent_plugins::process::{Spawn, SpawnOptions, Spawned};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use tokio::sync::watch;

#[derive(Default)]
struct MemoryStore(Mutex<HashMap<String, SavedSession>>);

impl SessionStore for MemoryStore {
    fn get(&self, key: &str) -> Option<SavedSession> {
        self.0.lock().unwrap().get(key).cloned()
    }
    fn set(&self, key: &str, session: SavedSession) {
        self.0.lock().unwrap().insert(key.into(), session);
    }
    fn delete(&self, key: &str) {
        self.0.lock().unwrap().remove(key);
    }
}

struct Pool {
    pool: AgentSessions,
    store: Arc<MemoryStore>,
    fake: FakeSpawn,
    model: Arc<Mutex<String>>,
    release: watch::Sender<bool>,
    spare: Option<Spare>,
}

impl Pool {
    fn claudes(&self) -> Vec<FakeClaude> {
        self.fake.claudes()
    }
    fn live(&self) -> Vec<FakeClaude> {
        self.fake.live()
    }
    fn set_model(&self, model: &str) {
        *self.model.lock().unwrap() = model.into();
    }
    fn deliver(&self, key: &str, text: &str, seen: u64) -> BoxFuture<'static, Settled> {
        self.pool
            .deliver(key.into(), text.into(), seen, DeliverOptions::default())
    }
    fn spare(&self) -> &Spare {
        self.spare.as_ref().expect("made with a spare")
    }
    /// `cwd` on this pool's host, with its model.
    fn at(&self, cwd: &str) -> Where {
        Where {
            spawn: self.fake.spawner(),
            cwd: cwd.into(),
            model: Some("opus".into()),
        }
    }
}

#[derive(Default)]
struct Options {
    store: Option<Arc<MemoryStore>>,
    /// Every process keeps its turns open until finished.
    hold: bool,
    /// Each launch waits, after reading the settings, until released, as a
    /// memory read does.
    held: bool,
    idle: Option<Duration>,
    /// The shared spare its new conversations look for.
    spare: Option<Spare>,
    tools: Option<ToolServer>,
    /// Another agent's processes on the same host.
    fake: Option<FakeSpawn>,
}

fn sessions(options: Options) -> Pool {
    let fake = options.fake.unwrap_or_else(|| FakeSpawn {
        hold: options.hold,
        ..Default::default()
    });
    let store = options.store.unwrap_or_default();
    let model = Arc::new(Mutex::new("opus".to_string()));
    let (release, released) = watch::channel(!options.held);
    let ids = Arc::new(AtomicUsize::new(0));
    let launch_model = model.clone();
    let fingerprint_model = model.clone();
    let tools = options.tools;
    let mut sessions_options = SessionsOptions::new(
        fake.spawner(),
        store.clone(),
        Arc::new(move || {
            let current = launch_model.lock().unwrap().clone();
            let mut released = released.clone();
            let tools = tools.clone();
            Box::pin(async move {
                let _ = released.wait_for(|released| *released).await;
                Ok(Launch {
                    cwd: "/work".into(),
                    system_prompt: "prompt".into(),
                    model: Some(current),
                    tools,
                    ..Default::default()
                })
            })
        }),
        Arc::new(move || fingerprint_model.lock().unwrap().clone()),
    );
    sessions_options.new_id =
        Arc::new(move || format!("new-{}", ids.fetch_add(1, Ordering::SeqCst) + 1));
    sessions_options.idle = options.idle.unwrap_or(IDLE);
    sessions_options.spare = options.spare.clone();
    Pool {
        pool: AgentSessions::new(sessions_options),
        store,
        fake,
        model,
        release,
        spare: options.spare,
    }
}

fn pool() -> Pool {
    sessions(Options::default())
}

/// A spare whose tools answer `{"from": "spare"}`, and whose sessions are
/// `spare-1`, `spare-2`, ….
fn spare() -> Spare {
    let ids = Arc::new(AtomicUsize::new(0));
    let tools: ToolServer = Arc::new(|_, _| async { Ok(json!({ "from": "spare" })) }.boxed());
    Spare::with_ids(
        tools,
        Arc::new(move || format!("spare-{}", ids.fetch_add(1, Ordering::SeqCst) + 1)),
    )
}

/// Sessions with a spare, spawned like their own processes.
fn with_spare(options: Options) -> Pool {
    sessions(Options {
        spare: Some(spare()),
        ..options
    })
}

fn tool(process: &FakeClaude, id: &str) {
    process.emit(json!({
        "type": "control_request", "request_id": id,
        "request": {
            "subtype": "mcp_message", "server_name": "buzz",
            "message": { "jsonrpc": "2.0", "id": 1, "method": "tools/list" },
        },
    }));
}

fn answered(process: &FakeClaude, id: &str) -> Option<Value> {
    process
        .responses()
        .into_iter()
        .find(|response| response["request_id"] == id)
}

/// A host whose processes never finish starting.
struct Never;

impl Spawn for Never {
    fn spawn(&self, _: &str, _: SpawnOptions) -> BoxFuture<'static, Result<Spawned, String>> {
        Box::pin(futures::future::pending())
    }
}

#[tokio::test]
async fn starts_a_new_conversation_on_the_shared_spare_and_warms_another() {
    let pool = with_spare(Options::default());
    pool.spare().warm(vec![pool.at("/work")]);
    flush().await;
    assert_eq!(pool.claudes().len(), 1);
    // Started before any agent asked: no prompt yet.
    assert!(pool.claudes()[0].state().received.is_empty());
    assert_eq!(pool.deliver("c/root", "hello", 10).await, Settled::ok());
    flush().await;
    assert_eq!(pool.claudes().len(), 2);
    let used = pool.claudes()[0].clone();
    let initialize = used.state().received[0].clone();
    assert_eq!(initialize["type"], "control_request");
    assert_eq!(initialize["request"]["subtype"], "initialize");
    assert_eq!(initialize["request"]["appendSystemPrompt"], "prompt");
    assert_eq!(used.prompts(), ["hello"]);
    let saved = pool.store.get("c/root").unwrap();
    assert_eq!((saved.id.as_str(), saved.seen), ("spare-1", 10));
    assert_eq!(
        pool.pool.snapshot(),
        vec![SessionView {
            key: "c/root".into(),
            busy: false
        }]
    );
}

#[tokio::test]
async fn hands_the_spares_tools_to_the_agent_that_takes_it() {
    let agent_tools: ToolServer = Arc::new(|_, _| async { Ok(json!({ "from": "agent" })) }.boxed());
    let pool = with_spare(Options {
        tools: Some(agent_tools),
        ..Options::default()
    });
    pool.spare().warm(vec![pool.at("/work")]);
    flush().await;
    let process = pool.claudes()[0].clone();
    // Claude lists the tools as it starts, before any agent has the spare.
    tool(&process, "early");
    flush().await;
    assert_eq!(
        answered(&process, "early").unwrap()["response"]["mcp_response"]["from"],
        "spare"
    );
    pool.deliver("c/root", "hi", 1).await;
    tool(&process, "later");
    flush().await;
    assert_eq!(
        answered(&process, "later").unwrap()["response"]["mcp_response"]["from"],
        "agent"
    );
}

#[tokio::test]
async fn serves_whichever_agent_starts_the_next_conversation() {
    let one = with_spare(Options::default());
    // Another agent on the same host.
    let two = sessions(Options {
        spare: one.spare.clone(),
        fake: Some(one.fake.clone()),
        ..Options::default()
    });
    one.spare().warm(vec![one.at("/work")]);
    flush().await;
    one.deliver("c/a", "for one", 1).await;
    flush().await;
    two.deliver("c/b", "for two", 2).await;
    // Each took the spare; neither agent started a process of its own.
    let prompts: Vec<Vec<String>> = one.claudes().iter().map(FakeClaude::prompts).collect();
    assert_eq!(
        prompts,
        vec![vec!["for one".to_string()], vec!["for two".into()], vec![]]
    );
}

#[tokio::test]
async fn starts_a_conversation_elsewhere_afresh_and_the_spare_follows_it() {
    let pool = with_spare(Options::default());
    pool.spare().warm(vec![pool.at("/other"), pool.at("/work")]);
    flush().await;
    let other = pool.claudes()[0].clone();
    pool.deliver("c/root", "hi", 1).await;
    flush().await;
    assert!(other.killed());
    assert!(other.prompts().is_empty());
    let used: Vec<FakeClaude> = pool
        .claudes()
        .into_iter()
        .filter(|process| !process.prompts().is_empty())
        .collect();
    assert!(used[0].args().contains(&"new-1".into()));
    let next = pool
        .claudes()
        .into_iter()
        .find(|process| process.args().contains(&"spare-2".into()))
        .unwrap();
    assert_eq!(next.state().options.cwd.as_deref(), Some("/work"));
    assert!(!next.killed());
}

#[tokio::test]
async fn does_not_wait_for_a_spare_started_elsewhere() {
    // The spare for another folder never finishes starting.
    let pool = with_spare(Options::default());
    pool.spare().warm(vec![Where {
        spawn: Arc::new(Never),
        cwd: "/other".into(),
        model: Some("opus".into()),
    }]);
    let settled = tokio::time::timeout(Duration::from_secs(5), pool.deliver("c/root", "hi", 1))
        .await
        .expect("waited for the spare");
    assert_eq!(settled, Settled::ok());
    assert_eq!(pool.claudes()[0].prompts(), ["hi"]);
}

#[tokio::test]
async fn uses_no_spare_started_on_another_host() {
    let pool = with_spare(Options::default());
    let elsewhere = FakeSpawn::default();
    pool.spare().warm(vec![Where {
        spawn: elsewhere.spawner(),
        ..pool.at("/work")
    }]);
    flush().await;
    let other = elsewhere.claudes()[0].clone();
    assert_eq!(pool.deliver("c/root", "hi", 1).await, Settled::ok());
    flush().await;
    assert!(other.killed());
    assert!(other.prompts().is_empty());
    assert_eq!(pool.claudes()[0].prompts(), ["hi"]);
    assert!(pool.claudes()[0].args().contains(&"new-1".into()));
    // A spare is still wanted on the other host.
    assert_eq!(elsewhere.live().len(), 1);
}

#[tokio::test]
async fn keeps_a_spare_that_fits_any_agent_and_replaces_one_that_fits_none() {
    let pool = with_spare(Options::default());
    let (work, other) = (pool.at("/work"), pool.at("/other"));
    pool.spare().warm(vec![work.clone()]);
    flush().await;
    pool.spare().warm(vec![other.clone(), work]);
    flush().await;
    assert_eq!(pool.claudes().len(), 1);
    pool.spare().warm(vec![other]);
    flush().await;
    assert_eq!(pool.claudes().len(), 2);
    assert!(pool.claudes()[0].killed());
    assert_eq!(
        pool.claudes()[1].state().options.cwd.as_deref(),
        Some("/other")
    );
}

#[tokio::test]
async fn starts_no_spare_again_after_one_dies_until_a_conversation_starts() {
    let pool = with_spare(Options::default());
    pool.spare().warm(vec![pool.at("/work")]);
    flush().await;
    pool.claudes()[0].exit(Some(1), Some("not signed in"));
    flush().await;
    pool.spare().warm(vec![pool.at("/work")]);
    flush().await;
    assert_eq!(pool.claudes().len(), 1);
    pool.deliver("c/root", "hi", 1).await;
    flush().await;
    // One for the conversation, and a spare again.
    assert_eq!(pool.claudes().len(), 3);
}

#[tokio::test]
async fn continues_a_conversation_in_its_own_process_apart_from_others() {
    let pool = pool();
    pool.deliver("c/one", "first", 1).await;
    pool.deliver("c/two", "other", 2).await;
    pool.deliver("c/one", "second", 3).await;
    let prompts: Vec<Vec<String>> = pool.claudes().iter().map(FakeClaude::prompts).collect();
    assert!(prompts.contains(&vec!["first".into(), "second".into()]));
    assert!(prompts.contains(&vec!["other".into()]));
}

#[tokio::test]
async fn steers_a_conversation_that_is_still_working_instead_of_queueing() {
    let pool = sessions(Options {
        hold: true,
        ..Options::default()
    });
    let first = tokio::spawn(pool.deliver("c/root", "first", 1));
    flush().await;
    let process = pool.claudes()[0].clone();
    let second = tokio::spawn(pool.pool.deliver(
        "c/root".into(),
        "plain".into(),
        2,
        DeliverOptions {
            steer: Some("steer".into()),
            ..Default::default()
        },
    ));
    flush().await;
    assert_eq!(process.prompts(), ["first", "steer"]);
    assert_eq!(
        pool.pool.snapshot(),
        vec![SessionView {
            key: "c/root".into(),
            busy: true
        }]
    );
    process.finish(None);
    assert_eq!(first.await.unwrap(), Settled::ok());
    assert_eq!(second.await.unwrap(), Settled::ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn opens_one_process_for_messages_that_arrive_together() {
    let pool = pool();
    let (one, two) = futures::join!(
        pool.deliver("c/root", "one", 1),
        pool.deliver("c/root", "two", 2)
    );
    assert!(one.ok && two.ok);
    let used: Vec<FakeClaude> = pool
        .claudes()
        .into_iter()
        .filter(|process| !process.prompts().is_empty())
        .collect();
    assert_eq!(used.len(), 1);
    assert_eq!(used[0].prompts(), ["one", "two"]);
}

#[tokio::test]
async fn resumes_a_saved_conversation_after_its_process_stops() {
    let pool = sessions(Options {
        idle: Some(Duration::from_millis(50)),
        ..Options::default()
    });
    pool.deliver("c/root", "first", 1).await;
    let first = pool.claudes()[0].clone();
    // The idle timer runs on the real clock; a busy machine may fire it
    // well after its 50ms.
    until(|| first.killed() && pool.pool.snapshot().is_empty()).await;
    pool.deliver("c/root", "later", 5).await;
    let resumed = pool.claudes().last().unwrap().clone();
    assert!(resumed.args().contains(&"--resume".into()));
    assert!(resumed.args().contains(&"new-1".into()));
    assert_eq!(resumed.prompts(), ["later"]);
    let saved = pool.store.get("c/root").unwrap();
    assert_eq!((saved.id.as_str(), saved.seen), ("new-1", 5));
}

#[tokio::test]
async fn starts_over_when_a_saved_session_no_longer_loads() {
    let store = Arc::new(MemoryStore::default());
    store.set(
        "c/root",
        SavedSession {
            id: "lost".into(),
            seen: 4,
            at: 1,
        },
    );
    let pool = sessions(Options {
        store: Some(store),
        ..Options::default()
    });
    let settled = pool
        .pool
        .deliver(
            "c/root".into(),
            "hello".into(),
            9,
            DeliverOptions {
                fresh: Some(Box::pin(async { "hello, with the thread so far".into() })),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(settled, Settled::ok());
    let claudes = pool.claudes();
    assert!(claudes[0].args().contains(&"lost".into()));
    assert!(claudes[1].args().contains(&"--session-id".into()));
    assert_eq!(claudes[1].prompts(), ["hello, with the thread so far"]);
    let saved = pool.store.get("c/root").unwrap();
    assert_eq!((saved.id.as_str(), saved.seen), ("new-1", 9));
}

#[tokio::test]
async fn runs_any_number_of_conversations_at_once() {
    let pool = sessions(Options {
        hold: true,
        ..Options::default()
    });
    for index in 0..12 {
        tokio::spawn(pool.deliver(&format!("c/{index}"), "hi", index));
    }
    flush().await;
    assert_eq!(pool.live().len(), 12);
    assert!(pool.pool.snapshot().iter().all(|session| session.busy));
}

fn once_prompt(text: Option<&str>) -> impl FnOnce() -> BoxFuture<'static, Option<String>> {
    let text = text.map(str::to_owned);
    move || Box::pin(async move { text })
}

#[tokio::test]
async fn runs_a_one_off_turn_in_a_new_session_that_is_never_saved() {
    let pool = pool();
    let settled = pool
        .pool
        .run_once("timer/a", once_prompt(Some("wake")), ONCE_DEADLINE)
        .await;
    assert!(settled.ok);
    let claude = pool
        .claudes()
        .into_iter()
        .find(|process| process.prompts().contains(&"wake".to_string()))
        .unwrap();
    assert!(claude.args().contains(&"--session-id".to_string()));
    assert!(claude.killed());
    assert!(pool.pool.snapshot().is_empty());
    assert!(pool.store.get("timer/a #1").is_none());
}

#[tokio::test]
async fn asks_a_one_off_turn_for_its_prompt_only_once_its_process_is_ready() {
    let pool = sessions(Options {
        held: true,
        ..Options::default()
    });
    // It went stale while its process started, as a watch turned off would.
    let stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let asked = Arc::new(AtomicUsize::new(0));
    let prompt = {
        let (stale, asked) = (stale.clone(), asked.clone());
        move || -> BoxFuture<'static, Option<String>> {
            asked.fetch_add(1, Ordering::SeqCst);
            let stale = stale.load(Ordering::SeqCst);
            Box::pin(async move { (!stale).then(|| "wake".to_string()) })
        }
    };
    let waiting = {
        let sessions = pool.pool.clone();
        tokio::spawn(async move { sessions.run_once("watch/a", prompt, ONCE_DEADLINE).await })
    };
    flush().await;
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    stale.store(true, Ordering::SeqCst);
    pool.release.send_replace(true);
    assert!(waiting.await.unwrap().ok);
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert!(
        !pool
            .claudes()
            .iter()
            .any(|process| process.prompts().contains(&"wake".to_string()))
    );
}

#[tokio::test]
async fn keeps_a_one_off_turn_until_its_process_has_exited_and_gives_up_at_its_deadline() {
    let pool = sessions(Options {
        hold: true,
        ..Options::default()
    });
    let done = {
        let sessions = pool.pool.clone();
        tokio::spawn(async move {
            sessions
                .run_once(
                    "timer/slow",
                    once_prompt(Some("work")),
                    Duration::from_millis(200),
                )
                .await
        })
    };
    until(|| {
        pool.claudes()
            .iter()
            .any(|process| process.prompts().contains(&"work".to_string()))
    })
    .await;
    let claude = pool
        .claudes()
        .into_iter()
        .find(|process| process.prompts().contains(&"work".to_string()))
        .unwrap();
    // The host only signals it; it exits later, as after a grace period.
    claude.state().exit_on_kill = false;
    until(|| claude.killed()).await;
    flush().await;
    assert!(!done.is_finished());
    let keys: Vec<String> = pool
        .pool
        .snapshot()
        .into_iter()
        .map(|session| session.key)
        .collect();
    assert_eq!(keys, ["timer/slow #1"]);
    claude.exit(None, None);
    let settled = done.await.unwrap();
    assert!(!settled.ok);
    assert!(
        settled
            .error
            .unwrap()
            .contains("ran past its 0s deadline and was stopped")
    );
    assert!(pool.pool.snapshot().is_empty());
}

#[tokio::test]
async fn starts_again_with_settings_saved_while_a_process_started() {
    let pool = sessions(Options {
        held: true,
        ..Options::default()
    });
    let done = tokio::spawn(pool.deliver("c/root", "hi", 1));
    flush().await;
    pool.set_model("sonnet");
    pool.pool.reconfigure();
    pool.release.send_replace(true);
    assert_eq!(done.await.unwrap(), Settled::ok());
    let claudes = pool.claudes();
    let old: Vec<&FakeClaude> = claudes
        .iter()
        .filter(|process| process.args().contains(&"opus".into()))
        .collect();
    assert_eq!(old.len(), 1);
    assert!(old[0].killed());
    let used: Vec<FakeClaude> = claudes
        .into_iter()
        .filter(|process| !process.prompts().is_empty())
        .collect();
    assert_eq!(used.len(), 1);
    assert!(used[0].args().contains(&"sonnet".into()));
}

#[tokio::test]
async fn restarts_idle_processes_when_settings_change() {
    let pool = pool();
    pool.deliver("c/root", "hi", 1).await;
    flush().await;
    let conversation = pool.claudes()[0].clone();
    pool.set_model("sonnet");
    pool.pool.reconfigure();
    flush().await;
    assert!(conversation.killed());
    pool.deliver("c/root", "again", 2).await;
    let args = pool.claudes().last().unwrap().args();
    for wanted in ["--resume", "new-1", "--model", "sonnet"] {
        assert!(args.contains(&wanted.into()), "{args:?}");
    }
}

#[tokio::test]
async fn stops_every_process_when_disposed() {
    let pool = with_spare(Options::default());
    pool.spare().warm(vec![pool.at("/work")]);
    pool.deliver("c/root", "hi", 1).await;
    flush().await;
    pool.pool.dispose();
    pool.spare().dispose();
    flush().await;
    assert_eq!(pool.claudes().len(), 2);
    assert!(pool.claudes().iter().all(FakeClaude::killed));
    assert!(!pool.deliver("c/root", "late", 2).await.ok);
    assert_eq!(pool.claudes().len(), 2);
}

#[tokio::test]
async fn starts_no_spare_for_a_conversation_that_starts_after_its_agents_are_gone() {
    let pool = with_spare(Options {
        held: true,
        ..Options::default()
    });
    pool.spare().warm(vec![pool.at("/work")]);
    let done = tokio::spawn(pool.deliver("c/root", "hi", 1));
    flush().await;
    // As the runtime does when its last agent goes, mid-launch.
    pool.pool.dispose();
    pool.spare().warm(vec![]);
    pool.release.send_replace(true);
    assert!(!done.await.unwrap().ok);
    flush().await;
    assert!(pool.live().is_empty());
}

#[test]
fn keeps_saved_sessions_per_agent_in_a_file() {
    let dir = tempfile_dir();
    let path = dir.join("sessions.json");
    let one = FileSessions::new(path.clone(), "one");
    let two = FileSessions::new(path.clone(), "two");
    let saved = |id: &str, seen, at| SavedSession {
        id: id.into(),
        seen,
        at,
    };
    one.set("c/root", saved("s1", 3, 1));
    two.set("c/root", saved("s2", 4, 2));
    assert_eq!(
        FileSessions::new(path.clone(), "one")
            .get("c/root")
            .unwrap()
            .id,
        "s1"
    );
    assert_eq!(two.get("c/root").unwrap().id, "s2");
    one.delete("c/root");
    assert_eq!(one.get("c/root"), None);
    assert_eq!(two.get("c/root").unwrap().id, "s2");
    // The newest 200 are kept.
    for index in 0..205 {
        one.set(&format!("c/{index}"), saved("s", 0, index));
    }
    assert_eq!(one.get("c/0"), None);
    assert!(one.get("c/204").is_some());
    // Saved in the background, newest last.
    let on_disk = || {
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<AllSessions>(&bytes).ok())
            .filter(|all| all.get("one").is_some_and(|one| one.contains_key("c/204")))
    };
    let start = std::time::Instant::now();
    while on_disk().is_none() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let all = on_disk().expect("the sessions were saved");
    assert_eq!(all["one"].len(), 200);
    assert_eq!(all["two"]["c/root"].id, "s2");
    // A file that does not parse holds no sessions.
    let broken = dir.join("broken.json");
    std::fs::write(&broken, "not json").unwrap();
    assert_eq!(FileSessions::new(broken, "one").get("c/root"), None);
    let _ = std::fs::remove_dir_all(dir);
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("buzz-claude-sessions-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
