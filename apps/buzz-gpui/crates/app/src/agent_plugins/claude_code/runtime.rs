//! Turns Agents2 deliveries into Claude turns, a port of buzz-app's
//! `runtime.ts`. Each agent of this type has its own sessions; a delivery
//! becomes a prompt in the harness format, sent to the session for its
//! conversation; a watch or timer wake runs as a one-off turn in a new
//! session, in its Interest's lane. The run returns once the turn is handed
//! over, so one agent
//! works in several conversations at once. Claude acts in Buzz with the
//! in-process Buzz tools, signed by the agent's host-held key; the runtime
//! only reports a turn that fails.

use super::claude::{ClaudeModel, Launch, Receipt, Settled, ToolServer, TurnPick};
use super::lanes::{Added, Lanes};
use super::prompt::{
    CONTEXT_LIMIT, Channel, Scope, Thread, attention_review, steer_prompt, system_prompt,
    timer_prompt, turn_prompt, watch_prompt,
};
use super::sessions::{
    AgentSessions, DeliverOptions, FileSessions, ONCE_DEADLINE, SessionStore, SessionView,
    SessionsOptions, Spare, Where,
};
use super::transcript::{Conversation, read_transcript};
use crate::agent_activity::local::{WorkHandle, WorkOutcome};
use crate::agent_plugins::harness::{self, MemoryReads};
use crate::agent_plugins::opened::Place;
use crate::agent_plugins::owner_relay::OwnerRelay;
use crate::agent_plugins::process::Spawner;
use crate::agents2::objects::ShownObject;
use crate::agents2::runtime::Current;
use crate::agents2::{Effort, Model, Models};
use crate::buzz_mcp::app_client::Signal;
use crate::buzz_mcp::app_client::{self, AgentActions, AppClient};
use crate::buzz_mcp::rpc::AttentionTools;
use crate::buzz_mcp::{BuzzClient, BuzzEvent, Context, Memory, Template, rpc, tools::hex};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::watch;

/// The type's config, as buzz-app saves it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// A Claude model or alias; empty uses Claude's own default.
    pub model: String,
    pub instructions: String,
    /// The agent's working directory, absolute or `~/…`.
    pub workspace: String,
    /// `thread` or `channel`: whether each thread is its own session, or
    /// the whole channel is one.
    pub scope: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: String::new(),
            instructions: String::new(),
            workspace: "~/.buzz".into(),
            scope: "thread".into(),
        }
    }
}

impl Config {
    /// The saved config over the defaults, field by field.
    pub fn read(value: &Value) -> Self {
        let mut config = Self::default();
        let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_string);
        if let Some(model) = field("model") {
            config.model = model;
        }
        if let Some(instructions) = field("instructions") {
            config.instructions = instructions;
        }
        if let Some(workspace) = field("workspace") {
            config.workspace = workspace;
        }
        if let Some(scope) = field("scope") {
            config.scope = scope;
        }
        config
    }

    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    pub fn scope(&self) -> Scope {
        if self.scope == "channel" {
            Scope::Channel
        } else {
            Scope::Thread
        }
    }

    /// What a running process fixed when it started.
    fn fingerprint(&self) -> String {
        json!([self.model, self.workspace, self.instructions, self.scope]).to_string()
    }

    /// Where an agent's processes run, on `spawn`'s host, and with which
    /// model.
    fn at(&self, spawn: &Spawner) -> Where {
        let model = self.model.trim();
        Where {
            spawn: spawn.clone(),
            cwd: self.workspace.clone(),
            model: (!model.is_empty()).then(|| model.to_string()),
        }
    }
}

const MEMORY_TIMEOUT: Duration = Duration::from_secs(5);
const NAMES_TIMEOUT: Duration = Duration::from_secs(2);
const CHAT: [u32; 2] = [9, 40002];

/// What a session can switch to between turns. Claude runs `max` as
/// `xhigh` in a stream-json session, so it is not offered.
/// The effort levels Claude lists, which a session can switch to between
/// turns, each once. Claude runs `max` as `xhigh` in a stream-json session,
/// so it is not offered.
fn efforts<'a>(levels: impl IntoIterator<Item = &'a String>) -> Vec<Effort> {
    let mut efforts: Vec<Effort> = Vec::new();
    for id in levels {
        if id == "max" || efforts.iter().any(|effort| effort.id == *id) {
            continue;
        }
        let label = if id == "xhigh" {
            "Extra high".to_string()
        } else {
            let mut chars = id.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        };
        efforts.push(Effort {
            id: id.clone(),
            label,
        });
    }
    efforts
}

/// `claude-opus-5-5` reads as `Opus 5.5`, and an alias such as `opus` as
/// `Opus`. Anything else is shown as it is.
pub fn model_name(model: &str) -> String {
    static ID: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^claude-([a-z]+)-(\d+)(?:-(\d{1,2}))?(?:-\d{8})?(?:\[.*\])?$")
            .expect("model id pattern")
    });
    let capitalized = |family: &str| {
        let mut chars = family.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect::<String>())
            .unwrap_or_default()
    };
    if let Some(id) = ID.captures(model) {
        let minor = id
            .get(3)
            .map_or(String::new(), |minor| format!(".{}", minor.as_str()));
        return format!("{} {}{minor}", capitalized(&id[1]), &id[2]);
    }
    if !model.is_empty() && model.chars().all(|c| c.is_ascii_lowercase()) {
        return capitalized(model);
    }
    model.to_string()
}

/// Why an agent woke, as this type needs it.
#[derive(Clone, Debug)]
pub enum Wake {
    Mention {
        event: BuzzEvent,
    },
    /// A watch matched `event`, observed data from anyone.
    Watch {
        event: BuzzEvent,
        slug: String,
        /// The Interest the watch serves: its lane.
        interest: String,
        /// Why its classifier did not run, when it has one.
        classifier: Option<String>,
    },
    Timer {
        slug: String,
        /// The Interest the timer serves: its lane.
        interest: String,
        /// This is its last allowed occurrence.
        spent: bool,
    },
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
    /// Told when the turn this run hands over settles.
    pub working: crate::agent_plugins::HoldUntil,
    /// Whether a watch's or timer's wake should still run.
    pub current: Current,
    /// A mention's local work row.
    pub activity: Option<WorkHandle>,
}

/// A settled turn as its request's outcome.
fn outcome(settled: Settled) -> WorkOutcome {
    if settled.ok {
        WorkOutcome::ok()
    } else {
        WorkOutcome::failed(settled.error.unwrap_or_default())
    }
}

/// One agent, as `sync` describes it.
pub struct Synced {
    pub pubkey: String,
    /// Its name, for its prompt; a rename restarts idle sessions.
    pub name: String,
    pub config: Value,
    pub spawn: Spawner,
    /// Whether the owner has attention on for it.
    pub attention: bool,
}

/// A saved session, for the owner's transcript list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedConversation {
    pub key: String,
    pub session_id: String,
    pub channel_id: String,
    pub root: Option<String>,
    /// The channel's name, when the agent's community lists it.
    pub name: Option<String>,
    pub at: u64,
}

impl SavedConversation {
    /// What reads the conversation's sessions, for `agent`.
    pub fn conversation(&self, agent: &str) -> Conversation {
        Conversation {
            agent: agent.into(),
            session_id: Some(self.session_id.clone()),
            channel_id: self.channel_id.clone(),
            root: self.root.clone(),
        }
    }
}

/// Where an agent's saved sessions are kept, by agent pubkey.
pub type StoreFn = Arc<dyn Fn(&str) -> Arc<dyn SessionStore> + Send + Sync>;

struct EntryState {
    config: Config,
    /// The agent's name, for its prompt; a rename restarts idle sessions.
    name: Option<String>,
    /// Whether the owner has attention on: new sessions get its tools, its
    /// guidance and the end-of-turn review.
    attention: bool,
    /// The team sections of the latest run; a change restarts idle
    /// sessions.
    team: String,
    /// The agent's handle from its latest delivery, for the tools to act
    /// with.
    handle: Option<Arc<dyn AgentActions>>,
    /// Where each conversation's tools default to: its latest turn's
    /// thread.
    contexts: HashMap<String, Context>,
    /// Each conversation's latest posting policy: when its turns were
    /// handed over, and when all of them settled.
    policies: HashMap<String, Policy>,
}

/// A conversation's posting policy while its turns run: a Me chat posts
/// at the channel's top level.
#[derive(Clone)]
struct Policy {
    top_level: bool,
    handed: Shared<BoxFuture<'static, ()>>,
    settled: Shared<BoxFuture<'static, ()>>,
    /// Tells this policy from a later one for the same conversation.
    id: u64,
}

static POLICIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Entry {
    sessions: AgentSessions,
    state: Arc<Mutex<EntryState>>,
    /// Watch and timer turns, one lane per Interest.
    lanes: Lanes,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Inner {
    agents: Mutex<HashMap<String, Arc<Entry>>>,
    memory_reads: MemoryReads,
    /// The owner's connection to each agent's own community, by agent
    /// pubkey.
    relays: Mutex<HashMap<String, Arc<dyn OwnerRelay>>>,
    /// One process started ahead of need, for any agent's next new
    /// conversation.
    spare: Spare,
    changes: watch::Sender<u64>,
    /// Where each agent's saved sessions are kept.
    store: StoreFn,
}

/// Every Claude agent's sessions. Cheap to clone.
#[derive(Clone)]
pub struct ClaudeRuntime {
    inner: Arc<Inner>,
}

impl ClaudeRuntime {
    /// Saved sessions go to `path`, a JSON file shared by all agents.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self::with_store(Arc::new(move |agent: &str| -> Arc<dyn SessionStore> {
            let path = path
                .clone()
                .or_else(FileSessions::default_path)
                .unwrap_or_else(|| PathBuf::from("claude-code-sessions.json"));
            Arc::new(FileSessions::new(path, agent))
        }))
    }

    pub fn with_store(store: StoreFn) -> Self {
        Self {
            inner: Arc::new(Inner {
                agents: Mutex::default(),
                memory_reads: MemoryReads::default(),
                relays: Mutex::default(),
                spare: Spare::new(Arc::new(|_, message: Value| {
                    async move { Ok(rpc::respond(None, &Context::default(), &message).await) }
                        .boxed()
                })),
                changes: watch::Sender::new(0),
                store,
            }),
        }
    }

    /// Each agent's own community, as its owner reads it, by agent pubkey.
    pub fn set_relays(&self, relays: HashMap<String, Arc<dyn OwnerRelay>>) {
        *locked(&self.inner.relays) = relays;
    }
    fn relay(&self, pubkey: &str) -> Option<Arc<dyn OwnerRelay>> {
        locked(&self.inner.relays).get(pubkey).cloned()
    }

    /// Bumps whenever a session starts, stops or settles.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }
    fn notify(&self) {
        self.inner.changes.send_modify(|count| *count += 1);
    }

    /// Brings the runtime in line with this type's agents: changed settings
    /// restart idle sessions, removed agents stop, and a spare waits while
    /// any agent is here. Must run inside tokio.
    pub fn sync(&self, agents: Vec<Synced>) {
        let current: HashSet<&str> = agents.iter().map(|agent| agent.pubkey.as_str()).collect();
        // Deleted, signed out or its community disconnected: either way its
        // processes stop. Saved sessions stay so it resumes when it is back.
        let gone: Vec<Arc<Entry>> = {
            let mut entries = locked(&self.inner.agents);
            let gone = entries
                .iter()
                .filter(|(pubkey, _)| !current.contains(pubkey.as_str()))
                .map(|(_, entry)| entry.clone())
                .collect();
            entries.retain(|pubkey, _| current.contains(pubkey.as_str()));
            gone
        };
        for entry in gone {
            entry.lanes.stop();
            entry.sessions.dispose();
        }
        let mut wheres = Vec::new();
        for agent in agents {
            let next = Config::read(&agent.config);
            wheres.push(next.at(&agent.spawn));
            let existing = locked(&self.inner.agents).get(&agent.pubkey).cloned();
            match existing {
                None => {
                    let entry = self.entry(&agent.pubkey, next, agent.spawn, agent.attention);
                    locked(&entry.state).name = Some(agent.name);
                }
                Some(entry) => {
                    let changed = {
                        let mut state = locked(&entry.state);
                        let changed = state.config != next
                            || state.name.as_ref() != Some(&agent.name)
                            || state.attention != agent.attention;
                        state.config = next;
                        state.name = Some(agent.name);
                        state.attention = agent.attention;
                        changed
                    };
                    // Attention changes what a new session is told and
                    // offered. Turned off, its waiting watch and timer
                    // turns are dropped too.
                    if !agent.attention {
                        entry.lanes.clear();
                    }
                    if changed {
                        entry.sessions.reconfigure();
                    }
                }
            }
        }
        self.inner.spare.warm(wheres);
        self.notify();
    }

    /// The models the agent's next turn can run on, with their effort
    /// levels. Without a pick it uses its own: the configured one, or else
    /// the one its sessions report. `default` duplicates a listed model, so
    /// it is not offered.
    pub async fn models(&self, pubkey: &str) -> Result<Models, String> {
        let entry = locked(&self.inner.agents).get(pubkey).cloned();
        let configured = entry
            .as_ref()
            .map(|entry| locked(&entry.state).config.model.trim().to_string())
            .unwrap_or_default();
        let metadata = match &entry {
            Some(entry) => entry.sessions.metadata().await?,
            None => Default::default(),
        };
        // A turn without a pick keeps the session's effort on any model.
        let resting = |efforts: &[Effort]| {
            metadata
                .effort
                .clone()
                .filter(|effort| efforts.iter().any(|level| level.id == *effort))
        };
        let own = if configured.is_empty() {
            metadata.model.clone().unwrap_or_default()
        } else {
            configured
        };
        let offered: Vec<_> = metadata
            .models
            .iter()
            .filter(|model| model.value != "default")
            .collect();
        let levels = |model: &ClaudeModel| -> Vec<String> {
            if model.supports_effort == Some(false) {
                Vec::new()
            } else {
                model.supported_effort_levels.clone().unwrap_or_default()
            }
        };
        let models: Vec<Model> = offered
            .iter()
            .map(|model| {
                let efforts = efforts(&levels(model));
                Model {
                    id: model.value.clone(),
                    label: model_name(model.resolved_model.as_deref().unwrap_or(&model.value)),
                    effort: resting(&efforts),
                    efforts,
                }
            })
            .collect();
        let current = offered
            .iter()
            .find(|model| own == model.value || Some(&own) == model.resolved_model.as_ref())
            .map_or(own.clone(), |model| model.value.clone());
        if models.iter().any(|model| model.id == current) {
            return Ok(Models { current, models });
        }
        // An own model Claude does not list is still the one it runs, at
        // any effort a listed model has.
        let all = efforts(
            &offered
                .iter()
                .flat_map(|model| levels(model))
                .collect::<Vec<_>>(),
        );
        let mut listed = vec![Model {
            id: current.clone(),
            label: if current.is_empty() {
                "Claude".into()
            } else {
                model_name(&current)
            },
            effort: resting(&all),
            efforts: all,
        }];
        listed.extend(models);
        Ok(Models {
            current,
            models: listed,
        })
    }

    /// Conversations each agent has running, for its status view.
    pub fn sessions(&self, pubkey: &str) -> Vec<SessionView> {
        locked(&self.inner.agents)
            .get(pubkey)
            .map(|entry| entry.sessions.snapshot())
            .unwrap_or_default()
    }

    /// The agent's saved sessions, most recently used first, for its
    /// owner's transcript list.
    pub fn conversations(&self, pubkey: &str) -> Vec<SavedConversation> {
        let relay = self.relay(pubkey);
        (self.inner.store)(pubkey)
            .list()
            .into_iter()
            .map(|(key, saved)| {
                let (channel_id, root) = match key.split_once('/') {
                    Some((channel, root)) => (channel.to_string(), Some(root.to_string())),
                    None => (key.clone(), None),
                };
                let name = relay
                    .as_ref()
                    .and_then(|relay| relay.channel(&channel_id)?.name);
                SavedConversation {
                    key,
                    session_id: saved.id,
                    channel_id,
                    root,
                    name,
                    at: saved.at,
                }
            })
            .collect()
    }

    /// The agent's latest session for a conversation that another copy of
    /// the app started, found in the Claude history of its workspace on
    /// the host `spawn` runs on.
    pub async fn find(
        &self,
        spawn: &Spawner,
        pubkey: &str,
        place: &Place,
        workspace: &str,
        signal: &Signal,
    ) -> Result<Option<SavedConversation>, String> {
        let conversation = Conversation {
            agent: pubkey.into(),
            session_id: None,
            channel_id: place.channel_id.clone(),
            root: place.root.clone(),
        };
        let read = read_transcript(spawn, workspace, &conversation, false, signal).await?;
        let Some(last) = read.and_then(|read| read.transcript.turns.last().cloned()) else {
            return Ok(None);
        };
        let Some(session_id) = last.session_id else {
            return Ok(None);
        };
        let name = self
            .relay(pubkey)
            .and_then(|relay| relay.channel(&place.channel_id)?.name);
        Ok(Some(SavedConversation {
            key: match &place.root {
                Some(root) => format!("{}/{root}", place.channel_id),
                None => place.channel_id.clone(),
            },
            session_id,
            channel_id: place.channel_id.clone(),
            root: place.root.clone(),
            name,
            at: last.ended_at.unwrap_or(last.started_at),
        }))
    }

    /// [`Self::dispose`], resolving once the processes it stops have
    /// exited.
    pub fn dispose_and_wait(&self) -> impl std::future::Future<Output = ()> + Send + use<> {
        let spare = self.inner.spare.process();
        let processes: Vec<_> = locked(&self.inner.agents)
            .values()
            .flat_map(|entry| entry.sessions.processes())
            .collect();
        self.dispose();
        async move {
            let spare = async {
                if let Some(spare) = spare
                    && let Some(process) = spare.await
                {
                    process.exited().await;
                }
            };
            let sessions = futures::future::join_all(processes.iter().map(|p| p.exited()));
            futures::join!(spare, sessions);
        }
    }

    pub fn dispose(&self) {
        self.inner.spare.dispose();
        let entries: Vec<Arc<Entry>> = locked(&self.inner.agents)
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in entries {
            entry.lanes.stop();
            entry.sessions.dispose();
        }
    }

    /// Hands one delivery to its conversation's session. Resolves once it
    /// is handed over, not when Claude has answered; fails when it cannot
    /// tell where to answer.
    pub async fn run(&self, run: Run) -> Result<(), String> {
        let settings = Config::read(&run.config);
        let pubkey = run.agent.pubkey();
        // Claude runs commands as you; mentions need owner or delegated
        // authority.
        let owner = run.agent.owner();
        // A watch is the agent's own attention: its event is observed data,
        // framed as such, from anyone.
        let trusted = match &run.wake {
            Wake::Mention { event } => {
                let relay = self.relay(&pubkey);
                crate::agent_plugins::from_owner_or_workflow(event, &owner, &pubkey, relay.as_ref())
                    .await
            }
            Wake::Watch { .. } | Wake::Timer { .. } => true,
        };
        if !trusted {
            // Declined: no work, and no outcome, to show.
            if let Some(work) = &run.activity {
                work.remove();
            }
            return Ok(());
        }
        let attention = match run.agent.attention() {
            Some(attention) => attention.enabled().await.unwrap_or(false),
            None => false,
        };
        let entry = self.entry(&pubkey, settings.clone(), run.spawn.clone(), attention);
        {
            let mut state = locked(&entry.state);
            state.handle = Some(run.agent.clone());
            state.team = run.team;
        }
        let event = match run.wake {
            Wake::Mention { event } => event,
            wake => {
                (run.working)(
                    self.wake(&entry, run.agent, wake, run.current)
                        .map(|()| WorkOutcome::ok())
                        .boxed(),
                );
                return Ok(());
            }
        };
        let Some(channel) = run.channel.or_else(|| tag(&event, "h")) else {
            return Ok(());
        };
        // A mention in the owner's Me chat posts at the channel's top level.
        let top_level = match self.relay(&pubkey) {
            Some(relay) => relay.me_chat(&owner, &channel).await?,
            None => false,
        };
        // Frozen with the policy: a scope change while this waits must not
        // move its prompt to another conversation than the one it is
        // delivered to.
        let key = self.key(&entry, &pubkey, &event, &channel, top_level);
        let prompt = {
            let (this, entry, event, channel, key) = (
                self.clone(),
                entry.clone(),
                event.clone(),
                channel.clone(),
                key.clone(),
            );
            move |fresh: bool| {
                let (this, entry, event, channel, key, pubkey) = (
                    this.clone(),
                    entry.clone(),
                    event.clone(),
                    channel.clone(),
                    key.clone(),
                    pubkey.clone(),
                );
                async move {
                    this.prompt(&entry, &pubkey, &event, &channel, &key, top_level, fresh)
                        .await
                }
                .boxed()
            }
        };
        // A turn keeps the posting policy it was admitted with: a request
        // under the other policy waits for the conversation's turns to
        // settle rather than steering into them and changing their tools'
        // context.
        let prior = locked(&entry.state).policies.get(&key).cloned();
        let same = prior
            .as_ref()
            .is_some_and(|prior| prior.top_level == top_level);
        let wait = prior.as_ref().map(|prior| {
            if same {
                prior.handed.clone()
            } else {
                prior.settled.clone()
            }
        });
        let waits = wait.is_some();
        let (handed_tx, handed_rx) = futures::channel::oneshot::channel::<()>();
        let done: Shared<BoxFuture<'static, Settled>> = {
            let (entry, channel, event, key) =
                (entry.clone(), channel.clone(), event.clone(), key.clone());
            let pick = TurnPick {
                model: run.model,
                effort: run.effort,
            };
            let activity = run.activity;
            async move {
                if let Some(wait) = wait {
                    wait.await;
                }
                let text = prompt(false).await;
                // A conversation's tools default to its thread; a session
                // shared by several threads is told which to reply in
                // instead.
                let root =
                    thread_reference(&event).map_or(event.id.clone(), |thread| thread.root_id);
                locked(&entry.state).contexts.insert(
                    key.clone(),
                    Context {
                        channel: Some(channel.clone()),
                        root: (key != channel && !top_level).then_some(root.clone()),
                        top_level,
                    },
                );
                let delivered = entry
                    .sessions
                    .deliver(
                        key,
                        text.clone(),
                        event.created_at,
                        DeliverOptions {
                            steer: Some(steer_prompt(&text)),
                            fresh: Some(prompt(true)),
                            pick,
                            receipt: activity.map(|work| Receipt { scope: root, work }),
                        },
                    )
                    .shared();
                // Queued in its conversation once first polled.
                let _ = delivered.clone().now_or_never();
                let _ = handed_tx.send(());
                delivered.await
            }
            .boxed()
            .shared()
        };
        tokio::spawn(done.clone());
        let handed = handed_rx.map(drop).boxed().shared();
        let settled = {
            let earlier = prior.filter(|_| same).map(|prior| prior.settled);
            let done = done.clone();
            async move {
                if let Some(earlier) = earlier {
                    earlier.await;
                }
                done.await;
            }
            .boxed()
            .shared()
        };
        let id = POLICIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        locked(&entry.state).policies.insert(
            key.clone(),
            Policy {
                top_level,
                handed: handed.clone(),
                settled: settled.clone(),
                id,
            },
        );
        {
            let state = entry.state.clone();
            tokio::spawn(async move {
                settled.await;
                let mut state = locked(&state);
                if state
                    .policies
                    .get(&key)
                    .is_some_and(|policy| policy.id == id)
                {
                    state.policies.remove(&key);
                }
            });
        }
        // Deliveries keep their order: a later one waits on this handover,
        // and one with nothing to wait for is handed over before `run`
        // returns.
        if !waits {
            handed.clone().await;
        }
        // The turn outlives `run`, which the host is told so it shows it as
        // working.
        (run.working)(done.clone().map(outcome).boxed());
        self.notify();
        self.watch(done.boxed(), Some((run.agent, channel, event, top_level)));
        Ok(())
    }

    /// A watch or timer wake: a one-off turn in its Interest's lane, as in
    /// Janet. It joins no conversation, and nothing about it is saved. The
    /// host says whether it is still `current`, asked when its lane starts
    /// it and again once it has a process: a wake whose watch or timer was
    /// disabled or removed, whose watch changed or no longer matches, or
    /// whose agent's attention went off, while it waited does not run. Its
    /// prompt is built then, from the attention as it is. Resolves when the
    /// turn ends or is dropped.
    fn wake(
        &self,
        entry: &Arc<Entry>,
        agent: Arc<dyn AgentActions>,
        wake: Wake,
        current: Current,
    ) -> BoxFuture<'static, ()> {
        let (kind, slug, lane) = match &wake {
            Wake::Watch { slug, interest, .. } => ("watch", slug.clone(), interest.clone()),
            Wake::Timer { slug, interest, .. } => ("timer", slug.clone(), interest.clone()),
            Wake::Mention { .. } => return async {}.boxed(),
        };
        let id = slug.strip_prefix("watch/").unwrap_or(&slug).to_owned();
        let (settle, settled) = tokio::sync::oneshot::channel::<()>();
        let settle = Arc::new(Mutex::new(Some(settle)));
        let done = {
            let settle = settle.clone();
            move || {
                if let Some(settle) = locked(&settle).take() {
                    let _ = settle.send(());
                }
            }
        };
        let dropped = {
            let done = done.clone();
            move |why: String| {
                tracing::warn!("Claude dropped a {kind} turn: {why}");
                done();
            }
        };
        let task = {
            let (this, sessions) = (self.clone(), entry.sessions.clone());
            async move {
                if !current().await {
                    return done();
                }
                let prompt = {
                    let (this, id) = (this.clone(), id.clone());
                    move || this.wake_prompt(agent, wake, id, current).boxed()
                };
                let result = sessions
                    .run_once(&format!("{kind}/{id}"), prompt, ONCE_DEADLINE)
                    .await;
                this.notify();
                if !result.ok {
                    let error = result.error.unwrap_or_default();
                    tracing::warn!(%error, "Claude {kind} turn failed: {id}");
                }
                done();
            }
            .boxed()
        };
        let on_drop = {
            let dropped = dropped.clone();
            Box::new(move || dropped("attention was turned off or the agent stopped".into()))
        };
        match entry.lanes.add(&lane, task, Some(on_drop)) {
            Added::Queued => {}
            Added::Full => dropped(format!("too many are waiting (Interest {lane})")),
            Added::Stopped => dropped("the agent stopped".into()),
        }
        // Dropped unsettled, as when its turn panicked, it has ended too.
        async move {
            let _ = settled.await;
        }
        .boxed()
    }

    /// The prompt for a wake as its watch or timer now is, or `None` when
    /// it should no longer run.
    async fn wake_prompt(
        self,
        agent: Arc<dyn AgentActions>,
        wake: Wake,
        id: String,
        current: Current,
    ) -> Option<String> {
        if !current().await {
            return None;
        }
        let attention = agent.attention()?;
        let (slug, timer) = match &wake {
            Wake::Watch { slug, .. } => (slug.clone(), false),
            Wake::Timer { slug, .. } => (slug.clone(), true),
            Wake::Mention { .. } => return None,
        };
        let object = attention.show(slug, None).await.ok()?.object?;
        let prompt = match &object {
            ShownObject::Timer { timer: value, .. } if timer => value.prompt.clone(),
            ShownObject::Event { .. } if !timer => String::new(),
            _ => return None,
        };
        let interest = object.value().interest_id().unwrap_or_default().to_owned();
        let instructions = match attention
            .show(format!("interest/{interest}"), None)
            .await
            .ok()
            .and_then(|shown| shown.object)
        {
            Some(ShownObject::Interest { interest, .. }) => Some(interest.instructions),
            _ => None,
        };
        let text = match &wake {
            Wake::Watch {
                event, classifier, ..
            } => watch_prompt(
                &id,
                &interest,
                instructions.as_deref(),
                event,
                classifier.as_deref(),
            ),
            _ => timer_prompt(
                &id,
                &interest,
                &prompt,
                instructions.as_deref(),
                matches!(wake, Wake::Timer { spent: true, .. }),
            ),
        };
        // Asked again: the attention can change while it is read.
        current()
            .await
            .then(|| format!("{text}\n\n{}", attention_review()))
    }

    /// `text` with Janet's end-of-turn attention review, when attention is
    /// on.
    fn review(entry: &Entry, text: String) -> String {
        if locked(&entry.state).attention {
            format!("{text}\n\n{}", attention_review())
        } else {
            text
        }
    }

    /// Notes when a turn settles, and says in the thread when it failed.
    fn watch(
        &self,
        done: BoxFuture<'static, Settled>,
        report: Option<(Arc<dyn AgentActions>, String, BuzzEvent, bool)>,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            let settled = done.await;
            if let (false, Some((agent, channel, event, top_level))) = (settled.ok, report) {
                let error = settled.error.unwrap_or_default();
                if let Err(failure) =
                    Self::report(agent.as_ref(), &channel, &event, &error, top_level).await
                {
                    tracing::warn!(%failure, "Claude could not report a failed turn");
                }
            }
            this.notify();
        });
    }

    /// Says in the thread that a turn failed, so nobody waits on it, or at
    /// the top level of a Me chat. Tells the owner even when a sibling
    /// asked.
    async fn report(
        agent: &dyn AgentActions,
        channel: &str,
        event: &BuzzEvent,
        error: &str,
        top_level: bool,
    ) -> Result<(), String> {
        let mut tags = crate::agent_plugins::reply_tags(channel, event, &agent.owner());
        if top_level {
            tags.retain(|tag| tag.first().map(String::as_str) != Some("e"));
        }
        let error: String = error.chars().take(500).collect();
        agent
            .publish(Template {
                kind: 9,
                content: format!("⚠️ I couldn't finish that: {error}"),
                tags,
            })
            .await
            .map(drop)
    }

    /// The conversation the event's turn goes to: a Me chat or DM is one
    /// conversation, as is a channel for an agent with channel scope.
    fn key(
        &self,
        entry: &Entry,
        own: &str,
        event: &BuzzEvent,
        channel_id: &str,
        top_level: bool,
    ) -> String {
        let dm = self
            .relay(own)
            .and_then(|relay| relay.channel(channel_id))
            .is_some_and(|summary| summary.dm);
        if top_level || dm || locked(&entry.state).config.scope() == Scope::Channel {
            channel_id.to_string()
        } else {
            let root = thread_reference(event).map_or(event.id.clone(), |thread| thread.root_id);
            format!("{channel_id}/{root}")
        }
    }

    /// The prompt for the event's turn in conversation `key`. `fresh` is
    /// for a session that has been shown nothing yet.
    #[allow(clippy::too_many_arguments)]
    async fn prompt(
        &self,
        entry: &Entry,
        own: &str,
        event: &BuzzEvent,
        channel_id: &str,
        key: &str,
        top_level: bool,
        fresh: bool,
    ) -> String {
        let relay = self.relay(own);
        let summary = relay.as_ref().and_then(|relay| relay.channel(channel_id));
        let dm = summary.as_ref().is_some_and(|summary| summary.dm);
        let thread = thread_reference(event);
        let root_id = thread
            .as_ref()
            .map_or(event.id.clone(), |thread| thread.root_id.clone());
        // The conversation it was admitted to, not the scope as it is now.
        let scope = if key == channel_id {
            Scope::Channel
        } else {
            Scope::Thread
        };
        let seen = if fresh { 0 } else { entry.sessions.seen(key) };
        let mut earlier: Vec<BuzzEvent> = Vec::new();
        if let Some(relay) = &relay
            && (thread.is_some() || dm || top_level)
        {
            let filters = if thread.is_some() && !top_level {
                vec![
                    json!({ "ids": [root_id], "limit": 1 }),
                    json!({ "kinds": CHAT, "#h": [channel_id], "#e": [root_id], "limit": 200 }),
                ]
            } else {
                vec![json!({ "kinds": CHAT, "#h": [channel_id], "limit": CONTEXT_LIMIT + 1 })]
            };
            match tokio::time::timeout(NAMES_TIMEOUT * 2, relay.read(filters)).await {
                Ok(Ok(events)) => {
                    let mut ids = HashSet::new();
                    earlier = events
                        .into_iter()
                        .filter(|item| {
                            CHAT.contains(&item.kind)
                                && item.id != event.id
                                && item.created_at <= event.created_at
                                && ids.insert(item.id.clone())
                        })
                        .collect();
                    earlier.sort_by_key(|item| item.created_at);
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "Claude could not read the conversation")
                }
                Err(_) => tracing::warn!("Claude timed out reading the conversation"),
            }
        }
        // A session already has its own replies; a new one is shown them too.
        let unseen: Vec<BuzzEvent> = earlier
            .iter()
            .filter(|item| item.created_at > seen && (seen == 0 || item.pubkey != own))
            .cloned()
            .collect();
        let mut pubkeys: Vec<String> = Vec::new();
        for key in std::iter::once(event.pubkey.clone())
            .chain(unseen.iter().map(|item| item.pubkey.clone()))
            .chain(
                event
                    .tags
                    .iter()
                    .filter(|tag| tag.first().map(String::as_str) == Some("p"))
                    .filter_map(|tag| tag.get(1).cloned()),
            )
        {
            if !key.is_empty() && !pubkeys.contains(&key) {
                pubkeys.push(key);
            }
        }
        let members = summary
            .as_ref()
            .map(|summary| summary.members.clone())
            .unwrap_or_default();
        let names = match &relay {
            Some(relay) => tokio::time::timeout(NAMES_TIMEOUT, relay.names(pubkeys, members))
                .await
                .unwrap_or_default(),
            None => HashMap::new(),
        };
        let channel = Channel {
            id: channel_id.to_string(),
            name: summary
                .and_then(|summary| summary.name)
                .filter(|name| !name.is_empty()),
            dm,
        };
        let name = |pubkey: &str| names.get(pubkey).cloned();
        let text = turn_prompt(&super::prompt::TurnInput {
            agent: own,
            event,
            channel: &channel,
            scope,
            top_level,
            thread: thread.as_ref(),
            context: &unseen,
            total: earlier.len() + 1,
            label: "@mention",
            name: &name,
        });
        Self::review(entry, text)
    }

    fn entry(&self, pubkey: &str, initial: Config, spawn: Spawner, attention: bool) -> Arc<Entry> {
        let mut agents = locked(&self.inner.agents);
        if let Some(existing) = agents.get(pubkey) {
            return existing.clone();
        }
        let state = Arc::new(Mutex::new(EntryState {
            config: initial,
            name: None,
            attention,
            team: String::new(),
            handle: None,
            contexts: HashMap::new(),
            policies: HashMap::new(),
        }));
        let launch = {
            let (this, state, pubkey, spawn) = (
                self.clone(),
                state.clone(),
                pubkey.to_string(),
                spawn.clone(),
            );
            Arc::new(move || {
                let (this, state, pubkey, spawn) =
                    (this.clone(), state.clone(), pubkey.clone(), spawn.clone());
                async move {
                    let (config, name, attention, team) = {
                        let state = locked(&state);
                        (
                            state.config.clone(),
                            state.name.clone(),
                            state.attention,
                            state.team.clone(),
                        )
                    };
                    let memory = this.core(&pubkey).await;
                    let Where { cwd, model, .. } = config.at(&spawn);
                    Ok(Launch {
                        cwd,
                        tools: Some(this.tools(
                            &pubkey,
                            &state,
                            &spawn,
                            &config.workspace,
                            attention,
                        )),
                        model,
                        system_prompt: system_prompt(
                            config.scope(),
                            &config.workspace,
                            name.as_deref().map(|name| (name, pubkey.as_str())),
                            &config.instructions,
                            &team,
                            memory.as_ref().map(|memory| memory.as_deref()),
                            attention,
                        ),
                        ..Default::default()
                    })
                }
                .boxed()
            })
        };
        let fingerprint = {
            let state = state.clone();
            Arc::new(move || {
                let state = locked(&state);
                // Tools plugins add are read when a session starts: a
                // change replaces idle sessions, which resume.
                json!([
                    state.config.fingerprint(),
                    state.name,
                    state.attention,
                    state.team,
                    crate::buzz_mcp::added::key()
                ])
                .to_string()
            })
        };
        let mut options =
            SessionsOptions::new(spawn, (self.inner.store)(pubkey), launch, fingerprint);
        options.spare = Some(self.inner.spare.clone());
        let changes = self.clone();
        options.on_change = Arc::new(move || changes.notify());
        let entry = Arc::new(Entry {
            sessions: AgentSessions::new(options),
            state,
            lanes: Lanes::default(),
        });
        agents.insert(pubkey.to_string(), entry.clone());
        entry
    }

    /// The tool server for processes started in `cwd`, which read files
    /// where they were started, and keep the tool list they were given,
    /// until they are replaced.
    fn tools(
        &self,
        pubkey: &str,
        state: &Arc<Mutex<EntryState>>,
        spawn: &Spawner,
        cwd: &str,
        attention: bool,
    ) -> ToolServer {
        let (this, state, pubkey, spawn, cwd) = (
            self.clone(),
            state.clone(),
            pubkey.to_string(),
            spawn.clone(),
            cwd.to_string(),
        );
        Arc::new(move |conversation: Option<String>, message: Value| {
            let (handle, context) = {
                let state = locked(&state);
                let context = conversation
                    .as_ref()
                    .and_then(|conversation| state.contexts.get(conversation).cloned())
                    .unwrap_or_default();
                (state.handle.clone(), context)
            };
            let tools = AttentionTools {
                offered: attention,
                api: handle.as_ref().and_then(|agent| agent.attention()),
            };
            let client: Option<Arc<dyn BuzzClient>> = handle.map(|agent| {
                let memories = {
                    let (this, pubkey) = (this.clone(), pubkey.clone());
                    Arc::new(move || {
                        this.memories(&pubkey)
                            .map(|read| read.map(|(memories, _)| memories))
                            .boxed()
                    }) as app_client::Memories
                };
                let remembered = {
                    let (this, pubkey) = (this.clone(), pubkey.clone());
                    Arc::new(move |_: &str| this.forget(&pubkey)) as app_client::Remembered
                };
                Arc::new(AppClient {
                    agent,
                    spawn: spawn.clone(),
                    cwd: cwd.clone(),
                    memories,
                    remembered,
                    check: None,
                    signal: None,
                }) as Arc<dyn BuzzClient>
            });
            async move { Ok(rpc::respond_with(client, &context, &message, Some(&tools)).await) }
                .boxed()
        })
    }

    /// The agent's `core` memory for a new session's prompt: its text,
    /// `Some(None)` when it has none, or `None` when it could not be read,
    /// so the session starts without it.
    async fn core(&self, pubkey: &str) -> Option<Option<String>> {
        match self.memories(pubkey).await {
            Ok((memories, partial)) => harness::core_of(&memories, partial),
            Err(error) => {
                tracing::warn!(%error, "Claude could not read the agent's memory");
                None
            }
        }
    }

    /// The agent's memory, read back by the owner in the agent's own
    /// community. Calls at once share one read.
    fn memories(
        &self,
        pubkey: &str,
    ) -> impl std::future::Future<Output = Result<(Vec<Memory>, bool), String>> + Send + 'static
    {
        let read = self
            .relay(pubkey)
            .map(|relay| self.inner.memory_reads.read(&relay, pubkey));
        async move {
            let read = read.ok_or("Buzz is not connected")?;
            tokio::time::timeout(MEMORY_TIMEOUT, read)
                .await
                .unwrap_or_else(|_| Err("Memory read timed out".into()))
        }
    }

    /// After a memory write, later reads start afresh.
    fn forget(&self, pubkey: &str) {
        if let Some(relay) = self.relay(pubkey) {
            self.inner.memory_reads.forget(&relay, pubkey);
        }
    }
}

/// NIP-10 as relay ingest reads it: the last valid marked root and reply
/// win; a lone root is not a reply.
pub fn thread_reference(event: &BuzzEvent) -> Option<Thread> {
    let (mut root, mut reply) = (None, None);
    for tag in &event.tags {
        let (Some(name), Some(value)) = (tag.first(), tag.get(1)) else {
            continue;
        };
        let value = value.to_lowercase();
        if name != "e" || !hex(&value) {
            continue;
        }
        match tag.get(3).map(String::as_str) {
            Some("root") => root = Some(value),
            Some("reply") => reply = Some(value),
            _ => {}
        }
    }
    let reply = reply?;
    Some(Thread {
        root_id: root.unwrap_or_else(|| reply.clone()),
        parent_id: reply,
    })
}

fn tag(event: &BuzzEvent, name: &str) -> Option<String> {
    crate::buzz_mcp::tools::tag(event, name)
}

#[cfg(test)]
mod tests;
