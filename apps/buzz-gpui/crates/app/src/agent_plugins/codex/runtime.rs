//! Turns Agents2 deliveries into Codex turns, a port of buzz-app's
//! `runtime.ts`. Agents2 owns admission and identity; this owns the work
//! after hand-over. Every agent on one host shares one `codex app-server`
//! there, and each conversation is its own Codex thread carrying its
//! agent's workspace, instructions and tools; tool calls find their agent
//! by thread and turn. A message that
//! arrives while its conversation works steers the running turn; one that
//! arrives while it is idle continues the saved thread. Codex answers with
//! the shared Buzz tools, signed by the agent's host-held key.

use super::config::{Config, DEFAULT_WORKSPACE, absolute_workspace, effort_name, valid_workspace};
use super::history::conversation_history;
use super::prompt::{
    Conversation, developer_instructions, root_of, session_name, turn_context, turn_input,
};
use super::rpc::{AppServer, Listener, ToolCall, ToolHandler, list_models};
use super::transcript::{Lookup, Place, find_conversation};
use crate::agent_activity::local::{WorkHandle, WorkOutcome};
use crate::agent_plugins::harness::{self, MemoryReads};
use crate::agent_plugins::owner_relay::{OwnerRelay, Scoped};
use crate::agent_plugins::process::{SpawnOptions, Spawner, collect};
use crate::agents2::{Effort as PickEffort, Model as PickModel, Models};
use crate::buzz_mcp::app_client::{self, AgentActions, AppClient, Check, Memories, Signal};
use crate::buzz_mcp::tools::{Reply, call_tool, tools};
use crate::buzz_mcp::{BuzzClient, BuzzEvent, Context, Memory, Template};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::{oneshot, watch};

/// Saved bindings per agent; older ones are forgotten, never Codex's own
/// history.
const SAVED_LIMIT: usize = 200;
/// Conversations each agent's status view keeps.
const VIEWS: usize = 32;
const DETAIL_LIMIT: usize = 4000;
/// The abort reasons of buzz-app's `AbortController` and `AbortSignal`.
const ABORTED: &str = "The operation was aborted.";
const TURN_ENDED: &str = "The Codex turn has ended";
const NOT_CONNECTED: &str = "Buzz is not connected to this community";
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Why an agent woke, as this type needs it.
#[derive(Clone, Debug)]
pub enum Wake {
    Mention {
        event: BuzzEvent,
    },
    Watch {
        event: BuzzEvent,
        /// The matched watch's Interest instructions.
        interest: Option<String>,
    },
    Timer,
}

/// One delivery.
pub struct Run {
    pub wake: Wake,
    /// The live route's channel.
    pub channel: Option<String>,
    pub agent: Arc<dyn AgentActions>,
    pub config: Value,
    /// Where the agent's processes run: the host that holds it.
    pub spawn: Spawner,
    /// The model and effort the owner picked for this turn.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The agent's team sections; empty when it is on no team.
    pub team: String,
    /// Told when the request this run hands over is answered.
    pub working: crate::agent_plugins::HoldUntil,
    /// A mention's local work row.
    pub activity: Option<WorkHandle>,
}

/// One conversation's state, for the Codex tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionView {
    pub key: String,
    pub status: String,
    pub detail: String,
}

/// Where saved bindings are kept: buzz-app's `localStorage`, one JSON
/// value per key.
pub trait Storage: Send + Sync {
    fn get(&self, key: &str) -> Option<Value>;
    fn set(&self, key: &str, value: Value) -> Result<(), String>;
}

/// Bindings in one JSON file, shared by every agent of the app.
pub struct FileStorage {
    path: PathBuf,
}

/// Agents of one app share the file; their writes take turns.
static FILE_LOCK: Mutex<()> = Mutex::new(());

impl FileStorage {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `<data dir>/buzz-gpui/codex-sessions.json`.
    pub fn default_path() -> PathBuf {
        dirs::data_dir()
            .map(|dir| dir.join("buzz-gpui").join("codex-sessions.json"))
            .unwrap_or_else(|| PathBuf::from("codex-sessions.json"))
    }

    fn read_all(&self) -> Map<String, Value> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
}

impl Storage for FileStorage {
    fn get(&self, key: &str) -> Option<Value> {
        let _turn = locked(&FILE_LOCK);
        self.read_all().remove(key)
    }

    fn set(&self, key: &str, value: Value) -> Result<(), String> {
        let _turn = locked(&FILE_LOCK);
        let mut all = self.read_all();
        all.insert(key.into(), value);
        let bytes = serde_json::to_vec(&all).map_err(|error| error.to_string())?;
        crate::atomic_file::write(&self.path, &bytes).map_err(|error| error.to_string())
    }
}

/// One request, as its turn or steering needs it.
#[derive(Clone)]
struct Request {
    /// The agent's community and owner, as `origin:viewer`.
    scope: String,
    event: BuzzEvent,
    agent: Arc<dyn AgentActions>,
    settings: Config,
    conversation: Conversation,
    /// The latest request's thread, whatever the session scope; `None` in
    /// DMs.
    tool_root: Option<String>,
    interest: String,
    /// The agent's team sections, if it is on a team.
    team: String,
    /// The model and effort the owner picked for this turn.
    model: Option<String>,
    effort: Option<String>,
    /// Called once a final send answers this request, or one steered
    /// after it, or it fails first.
    answered: Answered,
    /// A mention's work row: its acknowledgement, steps and supersession.
    activity: Option<WorkHandle>,
}

/// Settles a request's working hold once it is answered or has failed;
/// the first outcome wins. Cheap to clone.
#[derive(Clone)]
struct Answered(Arc<Mutex<Option<oneshot::Sender<WorkOutcome>>>>);

impl Answered {
    /// The answer, and the future that settles with it. A request dropped
    /// unanswered can no longer be answered: it ended without a reply.
    fn new() -> (Self, BoxFuture<'static, WorkOutcome>) {
        let (sender, receiver) = oneshot::channel();
        let replied = async move {
            receiver
                .await
                .unwrap_or_else(|_| WorkOutcome::failed(NO_REPLY))
        }
        .boxed();
        (Self(Arc::new(Mutex::new(Some(sender)))), replied)
    }

    fn answer(&self) {
        self.settle(WorkOutcome::ok());
    }

    /// The request failed: its bubble shows why.
    fn fail(&self, reason: &str) {
        self.settle(WorkOutcome::failed(reason));
    }

    fn settle(&self, outcome: WorkOutcome) {
        if let Some(sender) = locked(&self.0).take() {
            let _ = sender.send(outcome);
        }
    }
}

/// Why a request's work failed when its turn ended without answering it.
const NO_REPLY: &str = "Codex ended without a final reply";
const STOPPED: &str = "Codex was stopped";

/// A conversation's Codex thread, saved across restarts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    thread_id: String,
    workspace: String,
    /// When the binding last started a turn, in milliseconds; bindings
    /// saved before it existed have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    at: Option<u64>,
    /// [`crate::buzz_mcp::added::key`] when the thread started: a resumed
    /// thread keeps its tools, so other tools need a new thread.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    tools: String,
}

/// A saved binding, for the owner's transcript list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedConversation {
    pub key: String,
    pub channel_id: String,
    /// The channel's name, when the agent's community lists it.
    pub name: Option<String>,
    pub root: Option<String>,
    pub thread_id: String,
    pub at: Option<u64>,
}

/// The turn a conversation is running.
struct Active {
    /// The server this turn runs on; steering must reach the same process.
    rpc: AppServer,
    thread_id: Mutex<Option<String>>,
    turn_id: Mutex<Option<String>>,
    /// Settles once the turn has started, or failed to.
    ready: Shared<BoxFuture<'static, Result<(), String>>>,
    /// Follow-ups being steered in, in order.
    steering: Mutex<Shared<BoxFuture<'static, ()>>>,
    /// The latest accepted request, which tools reply to.
    reply: Mutex<Request>,
    finished: AtomicBool,
    /// What the turn runs on, once resolved; a differing pick queues a
    /// turn.
    model: Mutex<Option<String>>,
    effort: Mutex<Option<String>>,
    /// Where the turn started, for file tools.
    workspace: Mutex<String>,
    signal: Signal,
    /// Every request this turn has taken in, the first and each steered
    /// one.
    requests: Mutex<Vec<Request>>,
    /// The latest acknowledged mention per Buzz thread, whatever the
    /// session scope: the one that gets steps.
    activity: Mutex<HashMap<String, Request>>,
}

impl Active {
    /// Codex took `request`: its thread's activity moves to it, without
    /// claiming the earlier request succeeded. Watches have no bubble, and
    /// mentions in other threads keep theirs.
    fn acknowledge(&self, request: &Request) {
        let Some(work) = &request.activity else {
            return;
        };
        let prior = locked(&self.activity).insert(root_of(&request.event), request.clone());
        if let Some(prior) = prior.and_then(|prior| prior.activity) {
            prior.supersede();
        }
        work.started();
    }

    /// Every request taken in has failed with `reason`, unless answered.
    fn fail_all(&self, reason: &str) {
        for taken in locked(&self.requests).iter() {
            taken.answered.fail(reason);
        }
    }
}

impl Active {
    fn finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
    fn thread_id(&self) -> Option<String> {
        locked(&self.thread_id).clone()
    }
    fn turn_id(&self) -> Option<String> {
        locked(&self.turn_id).clone()
    }
    fn reply(&self) -> Request {
        locked(&self.reply).clone()
    }
}

/// One conversation's queue of turns.
struct Lane {
    /// The last job queued, by id.
    tail: Mutex<(u64, Shared<BoxFuture<'static, ()>>)>,
    /// Turns waiting on `tail` to start; steering past them would reorder.
    queued: AtomicU64,
    active: Mutex<Option<Arc<Active>>>,
}

impl Lane {
    /// A lane whose first turn waits for `after`.
    fn new(after: Shared<BoxFuture<'static, ()>>) -> Self {
        Self {
            tail: Mutex::new((0, after)),
            queued: AtomicU64::new(0),
            active: Mutex::default(),
        }
    }
    fn active(&self) -> Option<Arc<Active>> {
        locked(&self.active).clone()
    }
}

/// One agent's conversations.
struct Entry {
    pubkey: String,
    storage_key: String,
    abort: Signal,
    /// Settles once the same agent's previous run has let go of its threads.
    after: Shared<BoxFuture<'static, ()>>,
    lanes: Mutex<HashMap<String, Arc<Lane>>>,
    /// Bindings in the order they were first saved.
    saved: Mutex<Vec<(String, Saved)>>,
    spawn: Spawner,
}

impl Entry {
    fn saved(&self, key: &str) -> Option<Saved> {
        locked(&self.saved)
            .iter()
            .find(|(other, _)| other == key)
            .map(|(_, saved)| saved.clone())
    }
}

type Listing = Shared<BoxFuture<'static, Result<Models, String>>>;

/// Read-only model discovery for one agent, kept until its settings change.
struct Preview {
    key: String,
    host: usize,
    ready: Listing,
}

/// One host's `codex app-server`, shared by every agent there.
struct Server {
    rpc: AppServer,
    opening: Shared<BoxFuture<'static, Result<(), String>>>,
    /// Threads this process has loaded, and the agent each belongs to.
    threads: Mutex<HashMap<String, String>>,
}

struct Inner {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    /// By host: agents on one host share its server.
    servers: Mutex<HashMap<usize, Arc<Server>>>,
    /// Stopped agents still letting go of their threads.
    stopping: Mutex<HashMap<String, Shared<BoxFuture<'static, ()>>>>,
    /// By agent pubkey.
    previews: Mutex<HashMap<String, Preview>>,
    views: Mutex<HashMap<String, Vec<SessionView>>>,
    /// Each agent's own community, as its owner reads it, and its scope.
    relays: Mutex<HashMap<String, Scoped>>,
    storage: Arc<dyn Storage>,
    changes: watch::Sender<u64>,
    next_job: AtomicU64,
    memory_reads: MemoryReads,
}

/// Every Codex agent's conversations, and each host's server. Cheap to
/// clone.
#[derive(Clone)]
pub struct CodexRuntime {
    inner: Arc<Inner>,
}

fn input(text: String) -> Value {
    json!([{ "type": "text", "text": text, "text_elements": [] }])
}

/// The workspace as an absolute path. The host expands `~/…` and makes the
/// directory; Codex needs an absolute path rather than shell-style home notation.
async fn workspace(spawn: &Spawner, path: &str) -> Result<String, String> {
    if absolute_workspace(path) {
        return Ok(path.to_string());
    }
    let mut spawned = spawn
        .spawn(
            "workspace",
            SpawnOptions {
                cwd: Some(path.to_string()),
                ..Default::default()
            },
        )
        .await?;
    let (code, stdout, _) = collect(&mut spawned.output).await;
    let workspace = stdout.trim().to_string();
    if code != Some(0) || !absolute_workspace(&workspace) {
        return Err(format!("Could not resolve the Codex workspace {path}."));
    }
    Ok(workspace)
}

/// The request without the bot marker or the leading mention of the agent.
fn text_of(event: &BuzzEvent, name: &str) -> String {
    let mut text = event.content.trim();
    if let Some(rest) = text.strip_prefix('🤖')
        && rest.starts_with(char::is_whitespace)
    {
        text = rest.trim_start();
    }
    let mention = format!("@{name}");
    match text.strip_prefix(&mention) {
        Some(rest) => rest.trim().to_string(),
        None => text.to_string(),
    }
}

/// The `buzz` namespace of dynamic tools Codex is given.
fn dynamic_tools() -> Value {
    let tools: Vec<Value> = tools()
        .into_iter()
        .map(|tool| {
            let mut function = Map::new();
            function.insert("type".into(), json!("function"));
            if let Value::Object(tool) = tool {
                function.extend(tool);
            }
            Value::Object(function)
        })
        .collect();
    json!([{
        "type": "namespace",
        "name": "buzz",
        "description": "Read and act in Buzz as this agent.",
        "tools": tools,
    }])
}

fn text_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(value, |value, key| value.get(key))
        .and_then(Value::as_str)
}

impl CodexRuntime {
    /// Bindings go to `path`, a JSON file shared by all agents.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self::with_storage(Arc::new(FileStorage::new(
            path.unwrap_or_else(FileStorage::default_path),
        )))
    }

    pub fn with_storage(storage: Arc<dyn Storage>) -> Self {
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::default(),
                servers: Mutex::default(),
                stopping: Mutex::default(),
                previews: Mutex::default(),
                views: Mutex::default(),
                relays: Mutex::default(),
                storage,
                changes: watch::Sender::new(0),
                next_job: AtomicU64::new(1),
                memory_reads: MemoryReads::default(),
            }),
        }
    }

    /// The owner's connection to each agent's own community, and its
    /// scope, by agent pubkey.
    pub fn set_relays(&self, relays: HashMap<String, Scoped>) {
        *locked(&self.inner.relays) = relays;
    }

    /// The agent's community as its owner reads it, and its scope.
    fn relay(&self, pubkey: &str) -> Option<Scoped> {
        locked(&self.inner.relays).get(pubkey).cloned()
    }

    /// The agent's memory, and whether some records could not be read,
    /// read back through the owner's session: it is encrypted to the
    /// owner, so no agent key is needed to read it.
    async fn memories(
        &self,
        request: &Request,
        signal: &Signal,
    ) -> Result<(Vec<Memory>, bool), String> {
        let relay = self.relay_of(request).ok_or(NOT_CONNECTED)?;
        let read = self
            .inner
            .memory_reads
            .read(&relay, &request.agent.pubkey());
        let memories = tokio::select! {
            memories = read => memories?,
            reason = signal.wait() => return Err(reason),
        };
        // The community it was read from must still be the agent's.
        match self.relay_of(request) {
            Some(current) if Arc::ptr_eq(&current, &relay) => Ok(memories),
            _ => Err(NOT_CONNECTED.into()),
        }
    }

    /// The agent's `core` memory for a turn: its text, `Some(None)` when it
    /// has none, or `None` when it could not be read, so the turn goes on
    /// without it.
    async fn core(&self, request: &Request, signal: &Signal) -> Option<Option<String>> {
        match self.memories(request, signal).await {
            Ok((memories, partial)) => harness::core_of(&memories, partial),
            Err(error) => {
                if !signal.aborted() {
                    tracing::warn!(%error, "Codex could not read the agent's memory");
                }
                None
            }
        }
    }

    /// The request's community, while it is still the agent's.
    fn relay_of(&self, request: &Request) -> Option<Arc<dyn OwnerRelay>> {
        self.relay(&request.agent.pubkey())
            .filter(|(scope, _)| *scope == request.scope)
            .map(|(_, relay)| relay)
    }

    /// Bumps whenever a conversation's status changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }
    fn notify(&self) {
        self.inner.changes.send_modify(|count| *count += 1);
    }

    /// The models an agent's next turn can run on, with their effort
    /// levels, read from its host's server without starting a turn. Kept
    /// until the agent's settings change. Must run inside tokio.
    pub fn preview(&self, pubkey: &str, config: &Value, spawn: &Spawner) -> Listing {
        let settings = Config::read(config);
        let key = json!([settings.workspace, settings.model, settings.effort]).to_string();
        let host = spawn.host();
        if let Some(existing) = locked(&self.inner.previews)
            .get(pubkey)
            .filter(|preview| preview.key == key && preview.host == host)
        {
            return existing.ready.clone();
        }
        let server = self.open(spawn);
        let spawn = spawn.clone();
        let ready: Listing = async move {
            let workspace = workspace(&spawn, &settings.workspace).await?;
            server.opening.clone().await?;
            let rpc = &server.rpc;
            let normalized = rpc
                .request("config/read", json!({ "cwd": workspace }))
                .await?;
            let models = list_models(rpc).await?;
            let model = Some(settings.model.clone())
                .filter(|model| !model.is_empty())
                .or_else(|| {
                    text_at(&normalized, &["config", "model"])
                        .filter(|model| !model.is_empty())
                        .map(str::to_string)
                })
                .or_else(|| {
                    models
                        .iter()
                        .find(|row| row.is_default)
                        .map(|row| row.model.clone())
                });
            let offered: Vec<PickModel> = models
                .iter()
                .map(|row| {
                    let efforts: Vec<PickEffort> = row
                        .supported_reasoning_efforts
                        .iter()
                        .map(|level| PickEffort {
                            id: level.reasoning_effort.clone(),
                            label: effort_name(&level.reasoning_effort),
                        })
                        .collect();
                    // As a run picks it: the configured effort if the model
                    // has it.
                    let effort = if efforts.iter().any(|level| level.id == settings.effort) {
                        settings.effort.clone()
                    } else {
                        row.default_reasoning_effort.clone()
                    };
                    PickModel {
                        id: row.model.clone(),
                        label: if row.display_name.is_empty() {
                            row.model.clone()
                        } else {
                            row.display_name.clone()
                        },
                        efforts,
                        effort: (!effort.is_empty()).then_some(effort),
                    }
                })
                .collect();
            // A configured model this Codex does not list still leaves the
            // listed ones to pick, so the picker stays.
            Ok(match model {
                Some(model) if !models.iter().any(|row| row.model == model) => {
                    let mut listed = vec![PickModel {
                        id: model.clone(),
                        label: model.clone(),
                        efforts: Vec::new(),
                        effort: None,
                    }];
                    listed.extend(offered);
                    Models {
                        current: model,
                        models: listed,
                    }
                }
                model => Models {
                    current: model.unwrap_or_default(),
                    models: offered,
                },
            })
        }
        .boxed()
        .shared();
        locked(&self.inner.previews).insert(
            pubkey.to_string(),
            Preview {
                key,
                host,
                ready: ready.clone(),
            },
        );
        // A failed read is not kept, so the next one tries again.
        let (inner, pubkey, watched) = (
            Arc::downgrade(&self.inner),
            pubkey.to_string(),
            ready.clone(),
        );
        tokio::spawn(async move {
            if watched.clone().await.is_err()
                && let Some(inner) = inner.upgrade()
            {
                let mut previews = locked(&inner.previews);
                if previews
                    .get(&pubkey)
                    .is_some_and(|preview| preview.ready.ptr_eq(&watched))
                {
                    previews.remove(&pubkey);
                }
            }
        });
        ready
    }

    /// Each conversation's status, for the agent's Codex tab.
    pub fn sessions(&self, pubkey: &str) -> Vec<SessionView> {
        locked(&self.inner.views)
            .get(pubkey)
            .cloned()
            .unwrap_or_default()
    }

    fn status(&self, pubkey: &str, key: &str, status: &str, detail: &str) {
        // A stopped agent's draining turns don't bring its sessions back.
        if !locked(&self.inner.entries).contains_key(pubkey) {
            return;
        }
        {
            let mut views = locked(&self.inner.views);
            let views = views.entry(pubkey.into()).or_default();
            views.retain(|view| view.key != key);
            let excess = views.len().saturating_sub(VIEWS - 1);
            views.drain(..excess);
            views.push(SessionView {
                key: key.into(),
                status: status.into(),
                detail: detail.chars().take(DETAIL_LIMIT).collect(),
            });
        }
        self.notify();
    }

    /// Keeps the conversations of `pubkeys`, the agents still running
    /// whichever community is selected. An agent no longer present stops
    /// its turns; a host's server ends once none of its agents that has
    /// worked or listed models is left. Must run inside tokio.
    pub fn sync(&self, pubkeys: &[String]) {
        locked(&self.inner.previews).retain(|pubkey, _| pubkeys.contains(pubkey));
        let gone: Vec<Arc<Entry>> = {
            let mut entries = locked(&self.inner.entries);
            let gone = entries
                .iter()
                .filter(|(pubkey, _)| !pubkeys.contains(pubkey))
                .map(|(_, entry)| entry.clone())
                .collect();
            entries.retain(|pubkey, _| pubkeys.contains(pubkey));
            gone
        };
        for entry in gone {
            entry.abort.abort(ABORTED);
            locked(&self.inner.views).remove(&entry.pubkey);
            let server = locked(&self.inner.servers)
                .get(&entry.spawn.host())
                .cloned();
            if let Some(server) = server {
                self.stop(server, entry);
            }
        }
        let mut hosts: Vec<usize> = locked(&self.inner.entries)
            .values()
            .map(|entry| entry.spawn.host())
            .collect();
        hosts.extend(
            locked(&self.inner.previews)
                .values()
                .map(|preview| preview.host),
        );
        let idle: Vec<Arc<Server>> = {
            let mut servers = locked(&self.inner.servers);
            let idle = servers
                .iter()
                .filter(|(host, _)| !hosts.contains(host))
                .map(|(_, server)| server.clone())
                .collect();
            servers.retain(|host, _| hosts.contains(host));
            idle
        };
        for server in idle {
            drop(server.rpc.close());
        }
        self.notify();
    }

    /// Other agents share the server, so a stopped agent's turns interrupt
    /// themselves. Then the shells its finished turns left are ended,
    /// which its own process used to do by exiting. Until then, the same
    /// agent starting again waits rather than resuming a thread that is
    /// still letting go.
    fn stop(&self, server: Arc<Server>, entry: Arc<Entry>) {
        let pubkey = entry.pubkey.clone();
        let tails: Vec<Shared<BoxFuture<'static, ()>>> = locked(&entry.lanes)
            .values()
            .map(|lane| locked(&lane.tail).1.clone())
            .collect();
        let (inner, host) = (Arc::downgrade(&self.inner), entry.spawn.host());
        // A server that closed, as when the last agent stopped, ended its
        // shells with it.
        let current = move |server: &Arc<Server>| {
            inner.upgrade().is_some_and(|inner| {
                locked(&inner.servers)
                    .get(&host)
                    .is_some_and(|current| Arc::ptr_eq(current, server))
            })
        };
        let stopped = async move {
            futures::future::join_all(tails).await;
            let threads: Vec<String> = locked(&server.threads)
                .iter()
                .filter(|(_, owner)| **owner == entry.pubkey)
                .map(|(thread, _)| thread.clone())
                .collect();
            for thread_id in threads {
                if !current(&server) {
                    return;
                }
                locked(&server.threads).remove(&thread_id);
                if let Err(error) = server
                    .rpc
                    .request(
                        "thread/backgroundTerminals/clean",
                        json!({ "threadId": thread_id }),
                    )
                    .await
                {
                    tracing::error!(%error, "Codex could not clean a stopped agent's terminals");
                }
            }
        };
        let task = tokio::spawn(stopped);
        let stopped = async move {
            let _ = task.await;
        }
        .boxed()
        .shared();
        locked(&self.inner.stopping).insert(pubkey.clone(), stopped.clone());
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            stopped.clone().await;
            if let Some(inner) = inner.upgrade() {
                let mut stopping = locked(&inner.stopping);
                if stopping
                    .get(&pubkey)
                    .is_some_and(|current| current.ptr_eq(&stopped))
                {
                    stopping.remove(&pubkey);
                }
            }
        });
    }

    /// Ends every server. Must run inside tokio.
    pub fn dispose(&self) {
        drop(self.close_all());
    }

    /// [`Self::dispose`], resolving once every server has stopped. Must
    /// run inside tokio.
    pub fn dispose_and_wait(&self) -> impl std::future::Future<Output = ()> + Send + use<> {
        let closing = self.close_all();
        async move {
            futures::future::join_all(closing).await;
        }
    }

    fn close_all(&self) -> Vec<Shared<BoxFuture<'static, ()>>> {
        locked(&self.inner.previews).clear();
        for (_, entry) in locked(&self.inner.entries).drain() {
            entry.abort.abort(ABORTED);
        }
        locked(&self.inner.stopping).clear();
        // Closing tells the server's listeners, which take this lock.
        let servers: Vec<Arc<Server>> = locked(&self.inner.servers)
            .drain()
            .map(|(_, server)| server)
            .collect();
        let closing = servers.iter().map(|server| server.rpc.close()).collect();
        locked(&self.inner.views).clear();
        self.notify();
        closing
    }

    /// The host's server, opened on first use. Threads set their own
    /// working directory; the process just needs one that exists.
    fn open(&self, spawn: &Spawner) -> Arc<Server> {
        let host = spawn.host();
        let mut servers = locked(&self.inner.servers);
        if let Some(server) = servers.get(&host) {
            return server.clone();
        }
        let weak = Arc::downgrade(&self.inner);
        let tool: ToolHandler = {
            let runtime = weak.clone();
            Arc::new(move |call: ToolCall| {
                let runtime = runtime.upgrade();
                async move {
                    match runtime {
                        Some(inner) => CodexRuntime { inner }.tool(call).await,
                        None => Err("No active Buzz tool for this thread and turn".into()),
                    }
                }
                .boxed()
            })
        };
        let rpc = AppServer::new(Some(tool));
        let opening = {
            let (rpc, spawn) = (rpc.clone(), spawn.clone());
            let task =
                tokio::spawn(
                    async move { rpc.open(&spawn, Some(DEFAULT_WORKSPACE.to_string())).await },
                );
            async move {
                task.await
                    .unwrap_or_else(|_| Err("Codex connection closed".into()))
            }
            .boxed()
            .shared()
        };
        let server = Arc::new(Server {
            rpc,
            opening,
            threads: Mutex::default(),
        });
        // An exit fails current work and lets the next mention reopen.
        let closed: Listener = {
            let (runtime, this) = (weak.clone(), Arc::downgrade(&server));
            Arc::new(move |wire: &Value| {
                if wire.get("method").and_then(Value::as_str) == Some("connection/closed")
                    && let (Some(inner), Some(this)) = (runtime.upgrade(), this.upgrade())
                {
                    CodexRuntime { inner }.forget(host, &this);
                }
            })
        };
        server.rpc.on_message(closed);
        let failed = {
            let (runtime, this) = (self.clone(), server.clone());
            async move {
                if this.opening.clone().await.is_err() {
                    runtime.forget(host, &this);
                    this.rpc.close().await;
                }
            }
        };
        tokio::spawn(failed);
        servers.insert(host, server.clone());
        server
    }

    /// Drops `server` if it is still its host's.
    fn forget(&self, host: usize, server: &Arc<Server>) {
        let mut servers = locked(&self.inner.servers);
        if servers
            .get(&host)
            .is_some_and(|current| Arc::ptr_eq(current, server))
        {
            servers.remove(&host);
        }
    }

    fn storage_key(request: &Request) -> String {
        Self::key_of(&request.scope, &request.agent.pubkey())
    }

    fn key_of(scope: &str, pubkey: &str) -> String {
        // Resume restores the persisted tool schema, so a binding from
        // before a tool change starts fresh. v5 added channel and member
        // management.
        format!("buzz.codex.sessions.v5:{scope}:{pubkey}")
    }

    /// Saved bindings, in the order they were saved. Malformed local data
    /// starts fresh.
    fn load(&self, storage_key: &str) -> Vec<(String, Saved)> {
        match self.inner.storage.get(storage_key) {
            Some(Value::Object(stored)) => stored
                .into_iter()
                .filter_map(|(key, value)| Some((key, serde_json::from_value(value).ok()?)))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The agent's most recently used bindings in its community, for its
    /// owner's transcript list.
    pub fn conversations(&self, pubkey: &str) -> Vec<SavedConversation> {
        let relay = self.relay(pubkey);
        let entry = locked(&self.inner.entries).get(pubkey).cloned();
        let saved = match (entry, &relay) {
            (Some(entry), _) => locked(&entry.saved).clone(),
            (None, Some((scope, _))) => self.load(&Self::key_of(scope, pubkey)),
            (None, None) => Vec::new(),
        };
        // Untimestamped bindings keep save order, newest last; listed
        // newest first.
        let mut conversations: Vec<SavedConversation> = saved
            .into_iter()
            .rev()
            .filter_map(|(key, saved)| {
                let place: Vec<String> = serde_json::from_str(&key).ok()?;
                let [channel_id, root] = <[String; 2]>::try_from(place).ok()?;
                let name = relay
                    .as_ref()
                    .and_then(|(_, relay)| relay.channel(&channel_id)?.name);
                Some(SavedConversation {
                    key,
                    name,
                    root: (root != "channel").then_some(root),
                    channel_id,
                    thread_id: saved.thread_id,
                    at: saved.at,
                })
            })
            .collect();
        conversations.sort_by_key(|row| std::cmp::Reverse(row.at.unwrap_or(0)));
        conversations
    }

    /// The agent's latest thread for a conversation that another copy of
    /// the app started, found by the name the agent gave it, on the host
    /// `spawn` runs on. Must run inside tokio.
    pub async fn find(
        &self,
        spawn: &Spawner,
        agent: (&str, &str),
        place: &Place,
        signal: &Signal,
    ) -> Result<Option<SavedConversation>, String> {
        let (pubkey, agent_name) = agent;
        let name = self
            .relay(pubkey)
            .and_then(|(_, relay)| relay.channel(&place.channel_id)?.name);
        let lookup = Lookup {
            agent_pubkey: pubkey.into(),
            agent_name: agent_name.into(),
            channel_id: place.channel_id.clone(),
            name: name.clone(),
            root: place.root.clone(),
            thread_id: None,
        };
        let found = find_conversation(spawn, &lookup, signal).await?;
        Ok(found.map(|(thread_id, at)| SavedConversation {
            key: json!([place.channel_id, place.root.as_deref().unwrap_or("channel")]).to_string(),
            channel_id: place.channel_id.clone(),
            name,
            root: place.root.clone(),
            thread_id,
            at: Some(at),
        }))
    }

    fn entry(&self, request: &Request, spawn: &Spawner) -> Arc<Entry> {
        let pubkey = request.agent.pubkey();
        let mut entries = locked(&self.inner.entries);
        if let Some(prior) = entries.get(&pubkey) {
            return prior.clone();
        }
        let storage_key = Self::storage_key(request);
        let saved = self.load(&storage_key);
        let after = locked(&self.inner.stopping)
            .get(&pubkey)
            .cloned()
            .unwrap_or_else(|| futures::future::ready(()).boxed().shared());
        let entry = Arc::new(Entry {
            pubkey: pubkey.clone(),
            storage_key,
            abort: Signal::default(),
            after,
            lanes: Mutex::default(),
            saved: Mutex::new(saved),
            spawn: spawn.clone(),
        });
        entries.insert(pubkey, entry.clone());
        entry
    }
    fn save(&self, request: &Request, entry: &Entry, key: &str, saved: Saved) {
        let value = {
            let mut bindings = locked(&entry.saved);
            // Moved to the end, so the bounded set keeps the most recently
            // used bindings.
            bindings.retain(|(other, _)| other != key);
            bindings.push((key.into(), saved));
            let excess = bindings.len().saturating_sub(SAVED_LIMIT);
            bindings.drain(..excess);
            Value::Object(
                bindings
                    .iter()
                    .map(|(key, saved)| (key.clone(), json!(saved)))
                    .collect(),
            )
        };
        if let Err(error) = self.inner.storage.set(&entry.storage_key, value) {
            self.status(
                &request.agent.pubkey(),
                key,
                "Session could not be saved",
                &error,
            );
        }
    }

    /// Takes one delivery. Resolves once its work is handed over, not when
    /// Codex has answered. Must run inside tokio.
    pub async fn run(&self, run: Run) -> Result<(), String> {
        let Run {
            wake,
            channel,
            agent,
            config,
            spawn,
            model,
            effort,
            team,
            working,
            activity,
        } = run;
        let mention = matches!(wake, Wake::Mention { .. });
        let (event, interest) = match wake {
            Wake::Timer => {
                return Err(
                    "Codex timers are not supported yet; use mentions or event watches.".into(),
                );
            }
            Wake::Mention { event } => (event, String::new()),
            Wake::Watch { event, interest } => (event, interest.unwrap_or_default()),
        };
        let scoped = self.relay(&agent.pubkey());
        let trusted = crate::agent_plugins::from_owner_or_workflow(
            &event,
            &agent.owner(),
            &agent.pubkey(),
            scoped.as_ref().map(|(_, relay)| relay),
        )
        .await;
        if !trusted {
            // Declined: no work, and no outcome, to show.
            if let Some(work) = &activity {
                work.remove();
            }
            return Ok(());
        }
        let Some((scope, relay)) = scoped else {
            return Err("Connect to the agent's community first".into());
        };
        let channel_id = channel.ok_or("The event has no unambiguous channel")?;
        // Codex makes one more model call after a final send before its
        // turn ends; the agent is done working on this request once it has
        // replied.
        let (answered, replied) = Answered::new();
        working(replied);
        let settings = Config::read(&config);
        // Watches keep their routing; a mention in the owner's Me chat posts
        // at the channel's top level.
        let top_level = mention && relay.me_chat(&agent.owner(), &channel_id).await?;
        let info = relay.channel(&channel_id);
        let dm = info.as_ref().is_some_and(|info| info.dm);
        let tool_root = (!dm && !top_level).then(|| root_of(&event));
        let root = if settings.channel_scope() {
            None
        } else {
            tool_root.clone()
        };
        let request = Request {
            scope,
            event,
            agent,
            settings,
            tool_root,
            model,
            effort,
            answered: answered.clone(),
            activity,
            conversation: Conversation {
                name: info
                    .and_then(|info| info.name)
                    .unwrap_or_else(|| channel_id.clone()),
                channel_id: channel_id.clone(),
                root: root.clone(),
                top_level,
            },
            interest,
            team,
        };
        if !valid_workspace(&request.settings.workspace) {
            answered.fail("Choose an absolute workspace path or ~/.buzz in Codex settings first.");
            return self
                .publish(
                    &request,
                    "Choose an absolute or ~/… workspace path in Codex settings first.",
                )
                .await;
        }
        let key = json!([channel_id, root.as_deref().unwrap_or("channel")]).to_string();
        let text = text_of(&request.event, &request.agent.name());
        let entry = self.entry(&request, &spawn);
        // Held until the request is queued: a lane whose last turn just
        // ended is being removed, and a turn queued on it would run where
        // no tool call or later follow-up finds it.
        let mut lanes = locked(&entry.lanes);
        let lane = lanes
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Lane::new(entry.after.clone())))
            .clone();
        let entry = entry.clone();
        // A turn keeps the posting policy it was admitted with, so a
        // request under the other policy waits for it rather than steering
        // into it.
        let active = lane.active().filter(|active| {
            locked(&active.reply).conversation.top_level == request.conversation.top_level
        });
        match active {
            Some(active) => {
                // Startup and history reads stay ordered without holding the
                // delivery.
                let mut steering = locked(&active.steering);
                let previous = steering.clone();
                let (this, accepted, steered) = (self.clone(), active.clone(), lane.clone());
                let task = tokio::spawn(async move {
                    previous.await;
                    this.steer(request, text, key, entry, steered, accepted)
                        .await;
                });
                let accepted = async move {
                    if task.await.is_err() {
                        tracing::error!("Codex steering failed");
                    }
                }
                .boxed()
                .shared();
                *steering = accepted;
            }
            None => {
                drop(self.start(request, text, key, lane, entry));
            }
        }
        Ok(())
    }

    /// Steers `active` with a follow-up, or starts the next turn when it
    /// has finished meanwhile.
    async fn steer(
        &self,
        request: Request,
        text: String,
        key: String,
        entry: Arc<Entry>,
        lane: Arc<Lane>,
        active: Arc<Active>,
    ) {
        /// What became of the follow-up, when it did not fail.
        enum Steered {
            Done,
            Stopped,
            NextTurn,
        }
        let steered = async {
            let ready = active.ready.clone().await;
            if entry.abort.aborted() {
                return Ok(Steered::Stopped);
            }
            // A turn that failed to start is over too: its failure is
            // already posted, and the follow-up, often a retry sent after
            // reading it, starts afresh. The failed turn stays on the
            // lane until it has left its thread.
            // Steering cannot carry a model or effort, so a differing pick
            // waits for this turn and runs as the next one. Everything
            // after it queues too until the lane drains: steering would
            // run later messages first.
            let differs = |picked: &Option<String>, running: &Mutex<Option<String>>| {
                picked
                    .as_ref()
                    .is_some_and(|picked| locked(running).as_ref() != Some(picked))
            };
            if active.finished()
                || lane.queued.load(Ordering::SeqCst) > 0
                || differs(&request.model, &active.model)
                || differs(&request.effort, &active.effort)
            {
                return Ok(Steered::NextTurn);
            }
            ready?;
            let context = self.history(&request, &entry.abort).await?;
            if entry.abort.aborted() {
                return Ok(Steered::Stopped);
            }
            if active.finished() {
                return Ok(Steered::NextTurn);
            }
            let mut params = json!({
                "threadId": active.thread_id(),
                "input": input(format!(
                    "{context}{}",
                    turn_input(&request.event, &request.conversation, &text, &request.interest, true)
                )),
            });
            if let Some(turn) = active.turn_id() {
                params["expectedTurnId"] = json!(turn);
            }
            active.rpc.request("turn/steer", params).await?;
            *locked(&active.reply) = request.clone();
            locked(&active.requests).push(request.clone());
            active.acknowledge(&request);
            Ok::<_, String>(Steered::Done)
        }
        .await;
        match steered {
            Ok(Steered::Done) => {}
            Ok(Steered::Stopped) => request.answered.fail(STOPPED),
            Ok(Steered::NextTurn) => self.start_next(request, text, key, &entry),
            Err(_) if entry.abort.aborted() => request.answered.fail(STOPPED),
            // A completion racing the request means this is the next
            // ordinary turn.
            Err(error) if error == "no active turn to steer" => {
                self.start_next(request, text, key, &entry)
            }
            Err(error) => {
                request
                    .answered
                    .fail(&format!("Steering was not accepted: {error}"));
                let notice = format!(
                    "Steering was not accepted: {error}. Send another mention to continue."
                );
                if let Err(failure) = self.publish(&request, &notice).await {
                    tracing::error!(%failure, "Codex steering failed");
                }
                // The running turn never took this request in, so nothing
                // else answers it: let go of its working hold now, not at
                // that turn's end.
                request.answered.answer();
            }
        }
    }

    /// Starts a follow-up that came too late to steer. The turn it meant
    /// to steer has ended, so its lane may be gone or going: queue on the
    /// conversation's current lane, under the lock its removal takes.
    fn start_next(&self, request: Request, text: String, key: String, entry: &Arc<Entry>) {
        let mut lanes = locked(&entry.lanes);
        let lane = lanes
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Lane::new(entry.after.clone())))
            .clone();
        drop(self.start(request, text, key, lane, entry.clone()));
    }

    /// Queues a turn behind the conversation's last one. Callers hold the
    /// lane map's lock, so the lane can't be removed meanwhile.
    fn start(
        &self,
        request: Request,
        text: String,
        key: String,
        lane: Arc<Lane>,
        entry: Arc<Entry>,
    ) -> Shared<BoxFuture<'static, ()>> {
        let id = self.inner.next_job.fetch_add(1, Ordering::Relaxed);
        let pubkey = request.agent.pubkey();
        // Before the turn is spawned, which may report Working first.
        self.status(&pubkey, &key, "Starting", "");
        let job = {
            let mut tail = locked(&lane.tail);
            let previous = tail.1.clone();
            lane.queued.fetch_add(1, Ordering::SeqCst);
            let (this, key, lane, entry) = (self.clone(), key.clone(), lane.clone(), entry.clone());
            let task = tokio::spawn(async move {
                previous.await;
                lane.queued.fetch_sub(1, Ordering::SeqCst);
                if entry.abort.aborted() {
                    request.answered.fail(STOPPED);
                    return;
                }
                this.execute(request, text, key, lane, entry).await;
            });
            let job = async move {
                if task.await.is_err() {
                    tracing::error!("Codex turn failed");
                }
            }
            .boxed()
            .shared();
            *tail = (id, job.clone());
            job
        };
        // Agents2 can deliver follow-ups and other conversations at once.
        let done = job.clone();
        tokio::spawn(async move {
            job.await;
            // Checked under the map's lock, which queuing holds too.
            let mut lanes = locked(&entry.lanes);
            let idle = locked(&lane.tail).0 == id && lane.active().is_none();
            if idle
                && lanes
                    .get(&key)
                    .is_some_and(|current| Arc::ptr_eq(current, &lane))
            {
                lanes.remove(&key);
            }
        });
        done
    }

    /// The request's conversation before it, read as the owner.
    async fn history(&self, request: &Request, signal: &Signal) -> Result<String, String> {
        signal.check()?;
        let relay = self.relay_of(request).ok_or("The community changed")?;
        let read = move |filters: Vec<Value>| relay.read(filters);
        tokio::select! {
            history = conversation_history(&request.event, &request.conversation, &read) => history,
            reason = signal.wait() => Err(reason),
        }
    }

    /// Replies to the request in its thread, or at the top level of a Me
    /// chat, as the agent. Tags the owner even when a sibling asked.
    async fn publish(&self, request: &Request, content: &str) -> Result<(), String> {
        let mut tags = crate::agent_plugins::reply_tags(
            &request.conversation.channel_id,
            &request.event,
            &request.agent.owner(),
        );
        if request.conversation.top_level {
            tags.retain(|tag| tag.first().map(String::as_str) != Some("e"));
        }
        request
            .agent
            .publish(Template {
                kind: 9,
                content: content.into(),
                tags,
            })
            .await
            .map(drop)
    }

    /// Runs one Buzz tool for the turn that called it, whichever agent's
    /// it is.
    async fn tool(&self, call: ToolCall) -> Result<Reply, String> {
        let entries: Vec<Arc<Entry>> = locked(&self.inner.entries).values().cloned().collect();
        let found = entries.into_iter().find_map(|entry| {
            let active = locked(&entry.lanes)
                .values()
                .filter_map(|lane| lane.active())
                .find(|active| {
                    active.thread_id() == call.thread_id && active.turn_id() == call.turn_id
                })?;
            Some((entry, active))
        });
        let known = tools()
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some(call.tool.as_str()));
        let Some((entry, active)) =
            found.filter(|_| call.namespace.as_deref() == Some("buzz") && known)
        else {
            return Err("No active Buzz tool for this thread and turn".into());
        };
        let check: Check = {
            let active = active.clone();
            Arc::new(move || {
                active.signal.check()?;
                if active.finished() {
                    return Err(TURN_ENDED.into());
                }
                Ok(())
            })
        };
        check()?;
        let Value::Object(args) = call.arguments else {
            return Err("Buzz tool arguments must be an object".into());
        };
        let request = active.reply();
        let answering = locked(&active.requests).clone();
        let memories: Memories = {
            let (this, request, check, signal) = (
                self.clone(),
                request.clone(),
                check.clone(),
                active.signal.clone(),
            );
            Arc::new(move || {
                let (this, request, check, signal) =
                    (this.clone(), request.clone(), check.clone(), signal.clone());
                async move {
                    let (memories, _) = this.memories(&request, &signal).await?;
                    check()?;
                    Ok(memories)
                }
                .boxed()
            })
        };
        let remembered = {
            let (this, request) = (self.clone(), request.clone());
            Arc::new(move |_: &str| {
                if let Some(relay) = this.relay_of(&request) {
                    this.inner
                        .memory_reads
                        .forget(&relay, &request.agent.pubkey());
                }
            }) as app_client::Remembered
        };
        let client = AppClient {
            agent: request.agent.clone(),
            spawn: entry.spawn.clone(),
            cwd: locked(&active.workspace).clone(),
            memories,
            remembered,
            check: Some(check),
            signal: Some(active.signal.clone()),
        };
        let fin = call.tool == "send" && args.get("final") == Some(&Value::Bool(true));
        let context = Context {
            channel: Some(request.conversation.channel_id.clone()),
            root: request.tool_root.clone(),
            top_level: request.conversation.top_level,
        };
        let text = call_tool(
            Arc::new(client) as Arc<dyn BuzzClient>,
            &context,
            &call.tool,
            &args,
        )
        .await?;
        if fin {
            for taken in &answering {
                taken.answered.answer();
            }
        }
        Ok(text)
    }

    /// Stops the thread's background terminals. Interrupting a turn does
    /// not stop the shells it started, which run in their own groups.
    async fn terminals(&self, rpc: &AppServer, thread_id: &str) -> Result<(), String> {
        let cleaned = async {
            rpc.request(
                "thread/backgroundTerminals/clean",
                json!({ "threadId": thread_id }),
            )
            .await?;
            // `clean` acknowledges the request; terminating each shell
            // still reported is the barrier.
            let mut cursor = Value::Null;
            loop {
                let page = rpc
                    .request(
                        "thread/backgroundTerminals/list",
                        json!({ "threadId": thread_id, "limit": 100, "cursor": cursor }),
                    )
                    .await?;
                let terminals: Vec<String> = page
                    .get("data")
                    .and_then(Value::as_array)
                    .map(|data| {
                        data.iter()
                            .filter_map(|terminal| text_at(terminal, &["processId"]))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                futures::future::try_join_all(terminals.iter().map(|process| {
                    rpc.request(
                        "thread/backgroundTerminals/terminate",
                        json!({ "threadId": thread_id, "processId": process }),
                    )
                }))
                .await?;
                cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
                if cursor.is_null() {
                    break;
                }
            }
            let remaining = rpc
                .request(
                    "thread/backgroundTerminals/list",
                    json!({ "threadId": thread_id, "limit": 1 }),
                )
                .await?;
            if remaining
                .get("data")
                .and_then(Value::as_array)
                .is_some_and(|data| !data.is_empty())
            {
                return Err("Codex background terminals could not be stopped".to_string());
            }
            Ok(())
        }
        .await;
        if cleaned.is_err() {
            // A failed cleanup must not leave shells unreachable behind an
            // idle conversation.
            rpc.close().await;
        }
        cleaned
    }

    /// Runs one turn to its end, and says in the thread when it did not
    /// finish.
    async fn execute(
        &self,
        request: Request,
        text: String,
        key: String,
        lane: Arc<Lane>,
        entry: Arc<Entry>,
    ) {
        let signal = Signal::default();
        // The entry's end, or the turn's own end. No deadline of our own:
        // a turn runs until Codex ends it.
        let deadline = {
            let (signal, abort) = (signal.clone(), entry.abort.clone());
            tokio::spawn(async move {
                tokio::select! {
                    reason = abort.wait() => signal.abort(&reason),
                    _ = signal.wait() => {}
                }
            })
        };
        let (ready_sender, ready) = oneshot::channel::<Result<(), String>>();
        let ready_sender = Mutex::new(Some(ready_sender));
        let server = self.open(&entry.spawn);
        let active = Arc::new(Active {
            rpc: server.rpc.clone(),
            thread_id: Mutex::default(),
            turn_id: Mutex::default(),
            ready: async move { ready.await.unwrap_or_else(|_| Err(TURN_ENDED.into())) }
                .boxed()
                .shared(),
            steering: Mutex::new(futures::future::ready(()).boxed().shared()),
            reply: Mutex::new(request.clone()),
            requests: Mutex::new(vec![request.clone()]),
            activity: Mutex::default(),
            finished: AtomicBool::new(false),
            model: Mutex::default(),
            effort: Mutex::default(),
            workspace: Mutex::new(request.settings.workspace.clone()),
            signal: signal.clone(),
        });
        *locked(&lane.active) = Some(active.clone());
        let pubkey = request.agent.pubkey();
        let (settle, mut completed) = watch::channel::<Option<Result<String, String>>>(None);
        let settle = Arc::new(settle);
        let finish = {
            let settle = settle.clone();
            move |result: Result<String, String>| {
                settle.send_if_modified(|current| {
                    if current.is_some() {
                        return false;
                    }
                    *current = Some(result);
                    true
                });
            }
        };
        let aborted = {
            let (signal, finish) = (signal.clone(), finish.clone());
            tokio::spawn(async move { finish(Err(signal.wait().await)) })
        };
        let listener: Listener = {
            let (this, active, pubkey, key) =
                (self.clone(), active.clone(), pubkey.clone(), key.clone());
            Arc::new(move |wire: &Value| {
                let method = wire.get("method").and_then(Value::as_str);
                let params = wire.get("params").filter(|params| params.is_object());
                if method == Some("connection/closed") {
                    let message = params
                        .and_then(|params| text_at(params, &["message"]))
                        .unwrap_or("Codex connection closed");
                    return finish(Err(message.into()));
                }
                let Some(params) = params else {
                    return;
                };
                if text_at(params, &["threadId"]).map(str::to_string) != active.thread_id() {
                    return;
                }
                let turn = params.get("turn").filter(|turn| turn.is_object());
                let item = params.get("item").filter(|item| item.is_object());
                match (method, turn, item) {
                    (Some("turn/started"), Some(turn), _) => {
                        *locked(&active.turn_id) = text_at(turn, &["id"]).map(str::to_string);
                    }
                    (Some("item/started"), _, Some(item)) => {
                        if let Some((tool, target)) = codex_step(item) {
                            let taken: Vec<WorkHandle> = locked(&active.activity)
                                .values()
                                .filter_map(|request| request.activity.clone())
                                .collect();
                            for work in taken {
                                work.step(&tool, target.as_deref());
                            }
                        }
                        if let Some(command) = text_at(item, &["command"]).filter(|c| !c.is_empty())
                        {
                            this.status(&pubkey, &key, "Working", command);
                        }
                    }
                    (Some("item/completed"), _, Some(item))
                        if text_at(item, &["type"]) == Some("commandExecution") =>
                    {
                        let detail = format!(
                            "{}\n{}",
                            text_at(item, &["command"]).unwrap_or_default(),
                            text_at(item, &["aggregatedOutput"]).unwrap_or_default()
                        );
                        this.status(&pubkey, &key, "Working", &detail);
                    }
                    (Some("turn/completed"), Some(turn), _) => {
                        active.finished.store(true, Ordering::SeqCst);
                        let status = text_at(turn, &["status"]).unwrap_or_default();
                        finish(if status == "failed" {
                            Err(text_at(turn, &["error", "message"])
                                .unwrap_or("Codex turn failed")
                                .to_string())
                        } else {
                            Ok(status.to_string())
                        });
                    }
                    _ => {}
                }
            })
        };
        let listening = server.rpc.on_message(listener);
        let rpc = server.rpc.clone();
        let outcome = async {
            server.opening.clone().await?;
            signal.check()?;
            let workspace = workspace(&entry.spawn, &request.settings.workspace).await?;
            signal.check()?;
            *locked(&active.workspace) = workspace.clone();
            let existing = entry.saved(&key);
            let normalized = rpc
                .request("config/read", json!({ "cwd": workspace }))
                .await?;
            let models = list_models(&rpc).await?;
            let configured = text_at(&normalized, &["config", "model"]).filter(|m| !m.is_empty());
            let own = Some(request.settings.model.as_str())
                .filter(|model| !model.is_empty())
                .or(configured)
                .map(str::to_string)
                .or_else(|| {
                    models
                        .iter()
                        .find(|row| row.is_default)
                        .map(|row| row.model.clone())
                });
            // Model and effort are per turn, so a pick changes nothing
            // after it.
            let picked = models
                .iter()
                .find(|row| Some(&row.model) == request.model.as_ref());
            let Some(selected) =
                picked.or_else(|| models.iter().find(|row| Some(&row.model) == own.as_ref()))
            else {
                return Err(
                    "The selected Codex model is unavailable. Choose an available model in Settings."
                        .to_string(),
                );
            };
            let model = Some(selected.model.clone());
            // A picked model may not support the configured effort.
            let supported = |level: &str| {
                selected
                    .supported_reasoning_efforts
                    .iter()
                    .any(|effort| effort.reasoning_effort == level)
            };
            let effort = [request.effort.clone(), Some(request.settings.effort.clone())]
                .into_iter()
                .flatten()
                .find(|level| !level.is_empty() && supported(level))
                .unwrap_or_else(|| selected.default_reasoning_effort.clone());
            *locked(&active.model) = model.clone();
            *locked(&active.effort) = Some(effort.clone());
            // CLI overrides replace this table; disabled servers still need
            // a transport.
            let mcp: Map<String, Value> = normalized
                .pointer("/config/mcp_servers")
                .and_then(Value::as_object)
                .map(|servers| {
                    servers
                        .iter()
                        .map(|(name, server)| {
                            let mut disabled = Map::new();
                            match server.get("command") {
                                Some(Value::String(command)) => {
                                    disabled.insert("command".into(), json!(command));
                                }
                                _ => {
                                    if let Some(url) = server.get("url") {
                                        disabled.insert("url".into(), url.clone());
                                    }
                                }
                            }
                            disabled.insert("enabled".into(), json!(false));
                            (name.clone(), Value::Object(disabled))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let (name, agent) = (request.agent.name(), request.agent.pubkey());
            let params = json!({
                "model": model,
                "cwd": workspace,
                "developerInstructions": developer_instructions(&name, &agent),
                "approvalPolicy": "never",
                // Matches Claude's bypassPermissions; only the owner
                // can start work.
                "sandbox": "danger-full-access",
                "config": {
                    "web_search": "disabled",
                    "mcp_servers": mcp,
                    "features.plugins": false,
                    "features.apps": false,
                },
            });
            let with = |extra: Value| {
                let mut params = params.clone();
                if let (Some(params), Value::Object(extra)) = (params.as_object_mut(), extra) {
                    params.extend(extra);
                }
                params
            };
            let fresh = || with(json!({ "dynamicTools": dynamic_tools() }));
            let tools_key = crate::buzz_mcp::added::key();
            let resumed = existing
                .as_ref()
                .filter(|existing| existing.workspace == workspace && existing.tools == tools_key);
            let started = match resumed {
                Some(existing) => {
                    let resume = rpc
                        .request(
                            "thread/resume",
                            with(json!({ "threadId": existing.thread_id, "excludeTurns": true })),
                        )
                        .await;
                    match resume {
                        Err(error)
                            if error
                                == format!(
                                    "no rollout found for thread id {}",
                                    existing.thread_id
                                ) =>
                        {
                            rpc.request("thread/start", fresh()).await?
                        }
                        other => other?,
                    }
                }
                None => rpc.request("thread/start", fresh()).await?,
            };
            let thread_id = text_at(&started, &["thread", "id"])
                .ok_or("Codex did not start a thread")?
                .to_string();
            *locked(&active.thread_id) = Some(thread_id.clone());
            locked(&server.threads).insert(thread_id.clone(), pubkey.clone());
            signal.check()?;
            rpc.request(
                "thread/name/set",
                json!({
                    "threadId": thread_id,
                    "name": session_name(&name, &agent, &request.conversation),
                }),
            )
            .await?;
            let (context, memory) =
                tokio::join!(self.history(&request, &signal), self.core(&request, &signal));
            let context = context?;
            signal.check()?;
            // Before the request: Codex reports the turn's commands as soon
            // as it starts, sometimes before this task resumes with the
            // answer, and they must not be overwritten.
            self.status(&pubkey, &key, "Working", "");
            let turn = rpc
                .request(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": input(format!(
                            "{context}{}",
                            turn_input(&request.event, &request.conversation, &text, &request.interest, false)
                        )),
                        "model": model,
                        "effort": effort,
                        // Application context is a developer message that
                        // updates on each turn.
                        "additionalContext": turn_context(
                            &name,
                            &agent,
                            &request.settings,
                            &request.team,
                            memory.as_ref().map(Option::as_deref),
                        ),
                    }),
                )
                .await?;
            *locked(&active.turn_id) = text_at(&turn, &["turn", "id"]).map(str::to_string);
            active.acknowledge(&request);
            // An empty thread has no resumable rollout until a turn is
            // accepted.
            self.save(
                &request,
                &entry,
                &key,
                Saved {
                    thread_id: thread_id.clone(),
                    workspace,
                    at: Some(now_ms()),
                    tools: tools_key,
                },
            );
            if let Some(ready) = locked(&ready_sender).take() {
                let _ = ready.send(Ok(()));
            }
            let status = completed
                .wait_for(Option::is_some)
                .await
                .map_err(|_| TURN_ENDED.to_string())?
                .clone()
                .unwrap_or_else(|| Err(TURN_ENDED.into()))?;
            let steering = locked(&active.steering).clone();
            steering.await;
            signal.check()?;
            let reply = active.reply();
            if status == "interrupted" {
                active.fail_all("Codex’s turn was interrupted");
                self.terminals(&rpc, &thread_id).await?;
                self.publish(
                    &reply,
                    "Codex’s turn was interrupted. Send another mention to continue.",
                )
                .await?;
                self.status(&pubkey, &key, "Interrupted", "");
            } else {
                // A turn may end without a reply when there is nothing new
                // to say.
                for taken in locked(&active.requests).iter() {
                    taken.answered.answer();
                }
                self.status(&pubkey, &key, "Idle", "");
            }
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            active.fail_all(&error);
            let ended = active.finished.swap(true, Ordering::SeqCst);
            if let Some(ready) = locked(&ready_sender).take() {
                let _ = ready.send(Err(error.clone()));
            }
            let steering = locked(&active.steering).clone();
            steering.await;
            // Other agents share the server, so even a stopped agent
            // interrupts its turn rather than ending the process.
            if let (true, false, Some(thread_id), Some(turn_id)) = (
                signal.aborted(),
                ended,
                active.thread_id(),
                active.turn_id(),
            ) {
                let stopped = async {
                    rpc.request(
                        "turn/interrupt",
                        json!({ "threadId": thread_id, "turnId": turn_id }),
                    )
                    .await?;
                    self.terminals(&rpc, &thread_id).await
                }
                .await;
                if stopped.is_err() {
                    rpc.close().await;
                }
            }
            if !entry.abort.aborted() {
                let reason = format!("Codex could not finish: {error}.");
                if let Err(failure) = self.publish(&active.reply(), &reason).await {
                    tracing::error!(%failure, "Codex could not publish its failure");
                }
                self.status(&pubkey, &key, "Needs attention", &reason);
            }
        }
        active.fail_all(NO_REPLY);
        signal.abort(TURN_ENDED);
        deadline.abort();
        aborted.abort();
        rpc.off(listening);
        // Leave the thread before it is resumed: rejoining a loaded thread
        // ignores instruction and config overrides while a client is
        // subscribed.
        if let Some(thread_id) = active.thread_id()
            && rpc
                .request("thread/unsubscribe", json!({ "threadId": thread_id }))
                .await
                .is_err()
        {
            rpc.close().await;
        }
        let mut current = locked(&lane.active);
        if current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &active))
        {
            *current = None;
        }
    }
}

/// A started Codex item as a step, in the shared step vocabulary
/// (`codexStep`). Shapes follow the app-server v2 `ThreadItem` and
/// `CommandAction`.
pub fn codex_step(item: &Value) -> Option<(String, Option<String>)> {
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let file = |value: Option<&Value>| {
        text(value).and_then(|path| path.rsplit('/').next().map(str::to_string))
    };
    let step = |tool: &str, target: Option<String>| Some((tool.to_string(), target));
    match item.get("type").and_then(Value::as_str)? {
        "commandExecution" => {
            let action = item
                .get("commandActions")
                .and_then(Value::as_array)
                .and_then(|actions| actions.first());
            let get = |key: &str| action.and_then(|action| action.get(key));
            match get("type").and_then(Value::as_str) {
                Some("read") => step("Read", text(get("name")).or_else(|| file(get("path")))),
                Some("search") => step("Grep", text(get("query"))),
                Some("listFiles") => step("ListFiles", text(get("path"))),
                _ => step(
                    "Shell",
                    programs(
                        text(get("command"))
                            .or_else(|| text(item.get("command")))
                            .as_deref(),
                    ),
                ),
            }
        }
        "fileChange" => step(
            "Edit",
            file(
                item.get("changes")
                    .and_then(Value::as_array)
                    .and_then(|changes| changes.first())
                    .and_then(|change| change.get("path")),
            ),
        ),
        "mcpToolCall" | "dynamicToolCall" => {
            let tool = text(item.get("tool"))?;
            step(&tool, None)
        }
        "collabAgentToolCall" => step("Agent", text(item.get("prompt"))),
        "webSearch" => step("WebSearch", text(item.get("query"))),
        "imageView" => step("ImageView", file(item.get("path"))),
        "plan" => step("TodoWrite", None),
        "contextCompaction" => step("Compacting", None),
        "reasoning" => step("Thinking", None),
        "agentMessage" => step("Writing reply", None),
        _ => None,
    }
}

/// The programs a shell command runs ("mktemp, xargs"), without the
/// login-shell wrapper, arguments or quoted scripts (`programs`). The raw
/// command stays in the transcript.
pub fn programs(command: Option<&str>) -> Option<String> {
    let command = command?.trim();
    if command.is_empty() {
        return None;
    }
    let inner = shell_script(command).unwrap_or(command);
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in inner.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                current.push(c);
            }
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                current.push(c);
            }
            None if matches!(c, '|' | ';' | '&' | '\n') => {
                segments.push(std::mem::take(&mut current))
            }
            None => current.push(c),
        }
    }
    segments.push(current);
    let mut names: Vec<String> = Vec::new();
    for segment in &segments {
        let word = segment.split_whitespace().find(|token| !assignment(token));
        let name = word.map(|word| {
            word.trim_start_matches(['"', '\''])
                .trim_end_matches(['"', '\''])
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string()
        });
        if let Some(name) = name.filter(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || matches!(c, '.' | '+' | '-'))
        }) && !names.contains(&name)
        {
            names.push(name);
        }
    }
    match names.len() {
        0 => None,
        1..=3 => Some(names.join(", ")),
        n => Some(format!("{} and {} more", names[..3].join(", "), n - 3)),
    }
}

/// `VAR=value`, which is not the program.
fn assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The script of `[/path/]sh|bash|zsh -c|-lc '<script>'`.
fn shell_script(command: &str) -> Option<&str> {
    let (shell, rest) = command.split_once(char::is_whitespace)?;
    let shell = shell.rsplit('/').next()?;
    if !matches!(shell, "sh" | "bash" | "zsh") {
        return None;
    }
    let rest = rest.trim_start();
    let (flag, rest) = rest.split_once(char::is_whitespace)?;
    if !matches!(flag, "-c" | "-lc") {
        return None;
    }
    let rest = rest.trim();
    let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let body = rest.strip_prefix(quote)?.strip_suffix(quote)?;
    Some(body)
}

#[cfg(test)]
mod tests;
