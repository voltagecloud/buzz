//! One agent's Claude sessions, a port of buzz-app's `sessions.ts`: one per
//! conversation (a thread, or a whole DM), each continuing across messages
//! and app restarts. A conversation in use keeps its process; a quiet one is
//! stopped and resumed from its saved session when it is next addressed.
//! One spare process, shared by every agent, waits for the next new
//! conversation, so that starts without a spawn. A one-off turn (a watch or
//! timer wake) gets a new session of its own, which is never saved,
//! resumed or shared.

use super::claude::{ClaudeModel, ClaudeProcess, Launch, Receipt, Settled, ToolServer, TurnPick};
use crate::agent_plugins::process::Spawner;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

/// How long a one-off turn can run before it is given up; Janet's turn
/// deadline.
pub const ONCE_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// A conversation's process stops after this long without a message.
pub const IDLE: Duration = Duration::from_secs(15 * 60);
/// Saved sessions per agent; the least recently used are forgotten.
const SAVED_LIMIT: usize = 200;

/// What is saved for one conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSession {
    pub id: String,
    /// `created_at` of the newest event this session has been shown.
    pub seen: u64,
    /// When it was last used, in milliseconds.
    pub at: u64,
}

pub trait SessionStore: Send + Sync {
    fn get(&self, key: &str) -> Option<SavedSession>;
    fn set(&self, key: &str, session: SavedSession);
    fn delete(&self, key: &str);
    /// Saved sessions, most recently used first.
    fn list(&self) -> Vec<(String, SavedSession)> {
        Vec::new()
    }
}

pub type LaunchFn = Arc<dyn Fn() -> BoxFuture<'static, Result<Launch, String>> + Send + Sync>;

pub struct SessionsOptions {
    pub spawn: Spawner,
    /// Where a new conversation looks for a process already started.
    pub spare: Option<Spare>,
    pub store: Arc<dyn SessionStore>,
    /// How to start a process with the agent's current settings; `resume`
    /// and `session_id` are filled in here.
    pub launch: LaunchFn,
    /// Changes when a setting that a running process fixed has changed.
    pub fingerprint: Arc<dyn Fn() -> String + Send + Sync>,
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub new_id: Arc<dyn Fn() -> String + Send + Sync>,
    /// Called when a conversation's process starts or stops.
    pub on_change: Arc<dyn Fn() + Send + Sync>,
    pub idle: Duration,
}

impl SessionsOptions {
    /// Options with the clock, ids and idle time of a real agent.
    pub fn new(
        spawn: Spawner,
        store: Arc<dyn SessionStore>,
        launch: LaunchFn,
        fingerprint: Arc<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        Self {
            spawn,
            spare: None,
            store,
            launch,
            fingerprint,
            now: Arc::new(|| chrono::Utc::now().timestamp_millis().max(0) as u64),
            new_id: Arc::new(|| uuid::Uuid::new_v4().to_string()),
            on_change: Arc::new(|| {}),
            idle: IDLE,
        }
    }
}

struct Live {
    process: ClaudeProcess,
    fingerprint: String,
    used: Mutex<u64>,
    timer: Mutex<Option<tokio::task::AbortHandle>>,
}

type Starting = Shared<BoxFuture<'static, Option<Arc<Live>>>>;

/// What a session runs with when nothing is picked, and what it can switch
/// to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub models: Vec<ClaudeModel>,
    /// The model it runs, as Claude reports it.
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// A process started only to read [`Metadata`] for one set of settings.
struct Discovery {
    id: u64,
    fingerprint: String,
    result: Shared<BoxFuture<'static, Result<Metadata, String>>>,
    process: Arc<Mutex<Option<ClaudeProcess>>>,
}

#[derive(Default)]
struct State {
    discovery: Option<Discovery>,
    /// In the order they started, for the status view.
    live: Vec<(String, Arc<Live>)>,
    /// One-off turns running or starting, by label.
    once: Vec<(String, Option<Arc<Live>>)>,
    once_count: u64,
    opening: BTreeMap<String, (u64, Starting)>,
    /// Per conversation, the latest message's place: it resolves once
    /// that message has been handed to the process, or given up.
    queue: BTreeMap<String, (u64, Shared<BoxFuture<'static, ()>>)>,
    disposed: bool,
}

/// A message's place in its conversation's queue. Dropping it, once the
/// message is handed over or on any way out, lets the next one go.
struct Place {
    sessions: Weak<Inner>,
    key: String,
    id: u64,
    _done: tokio::sync::oneshot::Sender<()>,
}

impl Drop for Place {
    fn drop(&mut self) {
        if let Some(sessions) = upgrade(&self.sessions) {
            let mut state = sessions.state();
            if state
                .queue
                .get(&self.key)
                .is_some_and(|(id, _)| *id == self.id)
            {
                state.queue.remove(&self.key);
            }
        }
    }
}

impl State {
    fn get(&self, key: &str) -> Option<Arc<Live>> {
        self.live
            .iter()
            .find(|(other, _)| other == key)
            .map(|(_, live)| live.clone())
    }
    fn holds(&self, key: &str, live: &Arc<Live>) -> bool {
        self.get(key).is_some_and(|held| Arc::ptr_eq(&held, live))
    }
}

struct Inner {
    options: SessionsOptions,
    state: Mutex<State>,
    next: AtomicU64,
}

/// One agent's sessions. Cheap to clone; clones share everything.
#[derive(Clone)]
pub struct AgentSessions {
    inner: Arc<Inner>,
}

/// One conversation's running process, for the status view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionView {
    pub key: String,
    pub busy: bool,
}

/// Steering and restarting a delivery.
#[derive(Default)]
pub struct DeliverOptions {
    /// Replaces the text when the session is mid-turn.
    pub steer: Option<String>,
    /// Builds the text again for a new session when the saved one is lost.
    pub fresh: Option<BoxFuture<'static, String>>,
    /// The model and effort the turn was asked to run with.
    pub pick: TurnPick,
    /// A mention's work row, for Claude's acknowledgement and steps.
    pub receipt: Option<Receipt>,
}

/// What a process says it runs with, and can switch to.
async fn read_metadata(process: &ClaudeProcess) -> Metadata {
    let (models, model, effort) = futures::join!(
        process.models(),
        process.read_model(),
        process.read_effort()
    );
    Metadata {
        models,
        model,
        effort,
    }
}

fn kill_later(live: Arc<Live>) {
    kill_process(live.process.clone());
}

fn kill_process(process: ClaudeProcess) {
    tokio::spawn(async move { process.kill().await });
}

impl AgentSessions {
    pub fn new(options: SessionsOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                options,
                state: Mutex::default(),
                next: AtomicU64::new(1),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
    fn options(&self) -> &SessionsOptions {
        &self.inner.options
    }
    fn fingerprint(&self) -> String {
        (self.options().fingerprint)()
    }
    fn next_id(&self) -> u64 {
        self.inner.next.fetch_add(1, Ordering::Relaxed)
    }

    /// What `seen` says this conversation's session has been shown.
    pub fn seen(&self, key: &str) -> u64 {
        self.options().store.get(key).map_or(0, |saved| saved.seen)
    }

    /// Conversations with a running process, and whether each is working.
    pub fn snapshot(&self) -> Vec<SessionView> {
        let state = self.state();
        let live = state.live.iter().map(|(key, live)| SessionView {
            key: key.clone(),
            busy: live.process.busy(),
        });
        let once = state.once.iter().map(|(key, _)| SessionView {
            key: key.clone(),
            busy: true,
        });
        live.chain(once).collect()
    }

    /// The defaults and models of a session with the current settings,
    /// read without giving the shared spare to this agent: from a running
    /// session when one has them, else from a process started only to
    /// read them.
    pub async fn metadata(&self) -> Result<Metadata, String> {
        let fingerprint = self.fingerprint();
        let running = self
            .state()
            .live
            .iter()
            .find(|(_, live)| live.fingerprint == fingerprint && live.process.running())
            .map(|(_, live)| live.process.clone());
        if let Some(process) = running {
            let metadata = read_metadata(&process).await;
            let discovered = {
                let state = self.state();
                state
                    .discovery
                    .as_ref()
                    .filter(|discovery| discovery.fingerprint == fingerprint)
                    .map(|discovery| discovery.result.clone())
            };
            let discovered = match discovered {
                Some(result) => result.await.ok().and_then(|metadata| metadata.model),
                None => None,
            };
            return Ok(Metadata {
                model: metadata.model.or(discovered),
                ..metadata
            });
        }
        let result = {
            let mut state = self.state();
            if state.disposed {
                return Ok(Metadata::default());
            }
            match &state.discovery {
                Some(discovery) if discovery.fingerprint == fingerprint => discovery.result.clone(),
                _ => {
                    if let Some(old) = state.discovery.take() {
                        stop_discovery(old);
                    }
                    let id = self.next_id();
                    let process: Arc<Mutex<Option<ClaudeProcess>>> = Arc::default();
                    let result = {
                        let (this, holder) = (self.clone(), process.clone());
                        async move {
                            let stopped = || "Claude was stopped".to_string();
                            let current = |this: &AgentSessions| {
                                let state = this.state();
                                !state.disposed
                                    && state.discovery.as_ref().is_some_and(|d| d.id == id)
                            };
                            let launch = (this.options().launch)().await?;
                            if !current(&this) {
                                return Err(stopped());
                            }
                            let process = ClaudeProcess::start(
                                &this.options().spawn,
                                Launch {
                                    cwd: launch.cwd,
                                    model: launch.model,
                                    ..Default::default()
                                },
                            )
                            .await?;
                            *locked(&holder) = Some(process.clone());
                            let read = if current(&this) {
                                Ok(read_metadata(&process).await)
                            } else {
                                Err(stopped())
                            };
                            process.kill().await;
                            locked(&holder).take();
                            read
                        }
                        .boxed()
                        .shared()
                    };
                    state.discovery = Some(Discovery {
                        id,
                        fingerprint,
                        result: result.clone(),
                        process,
                    });
                    // A failed read is not kept, so the next one tries again.
                    let this = Arc::downgrade(&self.inner);
                    let watched = result.clone();
                    tokio::spawn(async move {
                        if watched.await.is_err()
                            && let Some(sessions) = upgrade(&this)
                        {
                            let mut state = sessions.state();
                            if state.discovery.as_ref().is_some_and(|d| d.id == id) {
                                state.discovery = None;
                            }
                        }
                    });
                    result
                }
            }
        };
        result.await
    }

    fn clear_discovery(&self) {
        if let Some(discovery) = self.state().discovery.take() {
            stop_discovery(discovery);
        }
    }

    /// Sends `text` into the conversation `key`, and resolves when the
    /// session has answered it.
    pub fn deliver(
        &self,
        key: String,
        text: String,
        seen: u64,
        options: DeliverOptions,
    ) -> BoxFuture<'static, Settled> {
        let this = self.clone();
        async move { this.deliver_now(key, text, seen, options).await }.boxed()
    }

    async fn deliver_now(
        &self,
        key: String,
        text: String,
        seen: u64,
        DeliverOptions {
            steer,
            fresh,
            pick,
            receipt,
        }: DeliverOptions,
    ) -> Settled {
        if self.state().disposed {
            return Settled::failed("Claude was stopped");
        }
        // Messages reach the process in the order they arrived, even when
        // they all wait for it to start and wake together.
        let (place, previous) = self.queue(&key);
        if let Some(previous) = previous {
            previous.await;
        }
        // A message that arrives while its conversation's process starts
        // uses it.
        let opening = self.state().opening.get(&key).map(|(_, open)| open.clone());
        if let Some(opening) = opening {
            opening.await;
        }
        let fingerprint = self.fingerprint();
        let mut live = self.state().get(&key);
        if let Some(current) = &live
            && (!current.process.running()
                || (current.fingerprint != fingerprint && !current.process.busy()))
        {
            self.drop_live(&key, current);
            live = None;
        }
        if let Some(live) = live {
            self.touch(&key, &live, Some(seen));
            let text = match (live.process.busy(), steer) {
                (true, Some(steer)) => steer,
                _ => text,
            };
            return self.answer(&key, &live, &text, pick, receipt, place).await;
        }
        let saved = self.options().store.get(&key);
        let id = self.next_id();
        let this = self.clone();
        let resume = saved.as_ref().map(|saved| saved.id.clone());
        let opened = key.clone();
        // Adopted as part of the start, so a message that waited on it finds
        // the conversation's process.
        let starting: Starting = async move {
            let live = match this.start(resume).await {
                Ok(live) => Some(live),
                Err(error) => {
                    tracing::warn!(%error, "Claude could not start");
                    None
                }
            };
            let disposed = {
                let mut state = this.state();
                if state
                    .opening
                    .get(&opened)
                    .is_some_and(|(other, _)| *other == id)
                {
                    state.opening.remove(&opened);
                }
                state.disposed
            };
            if let Some(live) = &live
                && !disposed
            {
                this.adopt(&opened, live, seen);
            }
            live
        }
        .boxed()
        .shared();
        self.state()
            .opening
            .insert(key.clone(), (id, starting.clone()));
        let Some(live) = starting.await else {
            return Settled::failed("Claude could not start");
        };
        if self.state().disposed {
            live.process.kill().await;
            return Settled::failed("Claude was stopped");
        }
        let delivered = self
            .answer(&key, &live, &text, pick.clone(), receipt.clone(), place)
            .await;
        // A saved session that no longer loads starts over rather than
        // failing every message in the conversation, shown the
        // conversation so far.
        if !delivered.ok && saved.is_some() && !live.process.started() {
            self.options().store.delete(&key);
            self.drop_live(&key, &live);
            let text = match fresh {
                Some(fresh) => fresh.await,
                None => text,
            };
            return self
                .deliver(
                    key,
                    text,
                    seen,
                    DeliverOptions {
                        pick,
                        receipt,
                        ..Default::default()
                    },
                )
                .await;
        }
        delivered
    }

    /// Runs one turn in a new session, then stops its process. The session
    /// is not saved, so it never displaces a conversation's saved session
    /// and is never resumed. `label` names it in the snapshot. It gives up,
    /// and stops the turn, after `deadline` of running. `prompt` is asked
    /// for the turn's text once a process is ready, so a turn that should
    /// no longer run by then answers `None` and sends nothing. It settles
    /// only when its process has exited, so it shows as working while it
    /// may still be working.
    pub async fn run_once(
        &self,
        label: &str,
        prompt: impl FnOnce() -> BoxFuture<'static, Option<String>>,
        deadline: Duration,
    ) -> Settled {
        let key = {
            let mut state = self.state();
            if state.disposed {
                return Settled::failed("Claude was stopped");
            }
            state.once_count += 1;
            let key = format!("{label} #{}", state.once_count);
            state.once.push((key.clone(), None));
            key
        };
        (self.options().on_change)();
        let mut held: Option<Arc<Live>> = None;
        let settled = async {
            let live = match self.start(None).await {
                Ok(live) => live,
                Err(error) => {
                    tracing::warn!(%error, "Claude could not start a one-off session");
                    return Settled::failed("Claude could not start");
                }
            };
            held = Some(live.clone());
            if self.state().disposed {
                return Settled::failed("Claude was stopped");
            }
            // A key no conversation uses: its tools have no default
            // destination.
            live.process.set_conversation(&key);
            if let Some((_, slot)) = self
                .state()
                .once
                .iter_mut()
                .find(|(other, _)| *other == key)
            {
                *slot = Some(live.clone());
            }
            let text = prompt().await;
            let Some(text) = text.filter(|_| !self.state().disposed) else {
                return Settled::ok();
            };
            let send = live.process.send(&text, TurnPick::default());
            match tokio::time::timeout(deadline, send).await {
                Ok(settled) => settled,
                Err(_) => Settled::failed(format!(
                    "The turn ran past its {}s deadline and was stopped",
                    deadline.as_secs_f64().round() as u64
                )),
            }
        }
        .await;
        if let Some(live) = held {
            // Killing only asks it to stop; the host may let it run a few
            // seconds.
            live.process.kill().await;
            live.process.exited().await;
        }
        self.state().once.retain(|(other, _)| *other != key);
        (self.options().on_change)();
        settled
    }

    /// Stops idle processes started with old settings; busy ones stop when
    /// they next fall idle.
    pub fn reconfigure(&self) {
        self.clear_discovery();
        let fingerprint = self.fingerprint();
        let stale: Vec<(String, Arc<Live>)> = self
            .state()
            .live
            .iter()
            .filter(|(_, live)| live.fingerprint != fingerprint && !live.process.busy())
            .cloned()
            .collect();
        for (key, live) in stale {
            self.drop_live(&key, &live);
        }
    }

    /// The processes it runs, so a caller can wait for them to exit.
    pub fn processes(&self) -> Vec<ClaudeProcess> {
        let state = self.state();
        let once = state.once.iter().filter_map(|(_, live)| live.as_ref());
        state
            .live
            .iter()
            .map(|(_, live)| live)
            .chain(once)
            .map(|live| live.process.clone())
            .collect()
    }

    pub fn dispose(&self) {
        self.clear_discovery();
        let (live, once) = {
            let mut state = self.state();
            state.disposed = true;
            (state.live.clone(), state.once.clone())
        };
        for (key, live) in live {
            self.drop_live(&key, &live);
        }
        for live in once.into_iter().filter_map(|(_, live)| live) {
            kill_later(live);
        }
    }

    async fn start(&self, resume: Option<String>) -> Result<Arc<Live>, String> {
        loop {
            // Read before `launch` reads the settings, so it never
            // describes newer ones.
            let fingerprint = self.fingerprint();
            let mut launch = (self.options().launch)().await?;
            let process = match &resume {
                Some(resume) => {
                    launch.resume = Some(resume.clone());
                    ClaudeProcess::start(&self.options().spawn, launch).await?
                }
                None => self.fresh(launch).await?,
            };
            if fingerprint == self.fingerprint() || self.state().disposed {
                return Ok(Arc::new(Live {
                    process,
                    fingerprint,
                    used: Mutex::new((self.options().now)()),
                    timer: Mutex::default(),
                }));
            }
            // Settings saved while it started: it would work with the old
            // ones.
            process.kill().await;
        }
    }

    /// A process for a new session: the spare when it fits, else a new one.
    async fn fresh(&self, mut launch: Launch) -> Result<ClaudeProcess, String> {
        let at = Where {
            spawn: self.options().spawn.clone(),
            cwd: launch.cwd.clone(),
            model: launch.model.clone(),
        };
        if let Some(spare) = &self.options().spare
            && let Some(process) = spare.take(&at).await
        {
            match process.initialize(&launch).await {
                Ok(()) => return Ok(process),
                Err(_) => kill_process(process),
            }
        }
        launch.session_id = Some((self.options().new_id)());
        ClaudeProcess::start(&self.options().spawn, launch).await
    }

    fn adopt(&self, key: &str, live: &Arc<Live>, seen: u64) {
        live.process.set_conversation(key);
        self.state().live.push((key.to_string(), live.clone()));
        self.touch(key, live, Some(seen));
        (self.options().on_change)();
        let this = Arc::downgrade(&self.inner);
        let (key, watched) = (key.to_string(), live.clone());
        tokio::spawn(async move {
            watched.process.exited().await;
            if let Some(sessions) = upgrade(&this)
                && sessions.state().holds(&key, &watched)
            {
                sessions.drop_live(&key, &watched);
            }
        });
    }

    /// Takes the next place in `key`'s queue, and the place before it, if
    /// one is still waiting to be handed its message.
    fn queue(&self, key: &str) -> (Place, Option<Shared<BoxFuture<'static, ()>>>) {
        let (done, handed) = tokio::sync::oneshot::channel::<()>();
        let id = self.next_id();
        let handed = async move {
            let _ = handed.await;
        }
        .boxed()
        .shared();
        let previous = self
            .state()
            .queue
            .insert(key.to_string(), (id, handed))
            .map(|(_, previous)| previous);
        let place = Place {
            sessions: Arc::downgrade(&self.inner),
            key: key.to_string(),
            id,
            _done: done,
        };
        (place, previous)
    }

    /// Hands the process `text`, then lets the next message in `key`'s
    /// queue go, and waits for the answer.
    async fn answer(
        &self,
        key: &str,
        live: &Arc<Live>,
        text: &str,
        pick: TurnPick,
        receipt: Option<Receipt>,
        place: Place,
    ) -> Settled {
        let settled = live.process.send_with(text, pick, receipt);
        drop(place);
        let settled = settled.await;
        if self.state().holds(key, live) {
            self.touch(key, live, None);
            if live.fingerprint != self.fingerprint() && !live.process.busy() {
                self.drop_live(key, live);
            }
        }
        settled
    }

    /// Saves the session and restarts its idle countdown.
    fn touch(&self, key: &str, live: &Arc<Live>, seen: Option<u64>) {
        let used = (self.options().now)();
        *live.used.lock().unwrap_or_else(PoisonError::into_inner) = used;
        let this = Arc::downgrade(&self.inner);
        let (owned_key, watched) = (key.to_string(), live.clone());
        let idle = self.options().idle;
        let timer = tokio::spawn(async move {
            tokio::time::sleep(idle).await;
            let Some(sessions) = upgrade(&this) else {
                return;
            };
            if watched.process.busy() {
                sessions.touch(&owned_key, &watched, None);
            } else if sessions.state().holds(&owned_key, &watched) {
                sessions.drop_live(&owned_key, &watched);
            }
        });
        if let Some(previous) = live
            .timer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(timer.abort_handle())
        {
            previous.abort();
        }
        let Some(id) = live.process.session_id() else {
            return;
        };
        let store = &self.options().store;
        let prior = store.get(key);
        let prior_seen = prior
            .filter(|prior| prior.id == id)
            .map_or(0, |prior| prior.seen);
        store.set(
            key,
            SavedSession {
                id,
                seen: seen.unwrap_or(0).max(prior_seen),
                at: used,
            },
        );
    }

    fn drop_live(&self, key: &str, live: &Arc<Live>) {
        if let Some(timer) = live
            .timer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            timer.abort();
        }
        self.state()
            .live
            .retain(|(other, held)| !(other == key && Arc::ptr_eq(held, live)));
        (self.options().on_change)();
        kill_later(live.clone());
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Kills a discovery's process, if it has one now.
fn stop_discovery(discovery: Discovery) {
    if let Some(process) = locked(&discovery.process).take() {
        kill_process(process);
    }
}

fn upgrade(inner: &Weak<Inner>) -> Option<AgentSessions> {
    inner.upgrade().map(|inner| AgentSessions { inner })
}

/// Where a process runs and its model: all a spare is started with. Each
/// agent's processes run on the host that holds it, so that is part of it.
#[derive(Clone)]
pub struct Where {
    pub spawn: Spawner,
    pub cwd: String,
    pub model: Option<String>,
}

impl Where {
    fn place(&self) -> String {
        json!([
            self.spawn.host(),
            self.cwd,
            self.model.as_deref().unwrap_or(""),
            crate::buzz_mcp::added::key()
        ])
        .to_string()
    }
}

type SpareProcess = Shared<BoxFuture<'static, Option<ClaudeProcess>>>;

struct Next {
    id: u64,
    place: String,
    process: SpareProcess,
}

#[derive(Default)]
struct SpareState {
    next: Option<Next>,
    /// Where a spare is wanted: set by the runtime alone.
    wanted: Vec<Where>,
    /// Not started again after it failed to start or died, until a
    /// conversation asks for one, so a `claude` that cannot run is not
    /// started over and over.
    stopped: bool,
}

struct SpareInner {
    /// Answers the tool server before any agent has it: it lists the tools.
    tools: ToolServer,
    new_id: Arc<dyn Fn() -> String + Send + Sync>,
    state: Mutex<SpareState>,
    next: AtomicU64,
}

/// One process started ahead of need, for whichever agent next starts a
/// conversation where it runs and with its model. Starting is most of the
/// wait before a new conversation's first reply; giving the process its
/// prompt is not. It is kept only where the runtime's agents run, and
/// follows the latest new conversation among those places, the likeliest
/// place for the next. Cheap to clone; clones are the same spare.
#[derive(Clone)]
pub struct Spare {
    inner: Arc<SpareInner>,
}

fn kill_spare(process: SpareProcess) {
    tokio::spawn(async move {
        if let Some(process) = process.await {
            process.kill().await;
        }
    });
}

impl Spare {
    pub fn new(tools: ToolServer) -> Self {
        Self::with_ids(tools, Arc::new(|| uuid::Uuid::new_v4().to_string()))
    }

    pub fn with_ids(tools: ToolServer, new_id: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(SpareInner {
                tools,
                new_id,
                state: Mutex::default(),
                next: AtomicU64::new(1),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, SpareState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Wants a spare for one of `wheres`, or none when there are none. Must
    /// run inside tokio.
    pub fn warm(&self, wheres: Vec<Where>) {
        let mut state = self.state();
        if wheres.is_empty() {
            state.stopped = false;
        }
        state.wanted = wheres.clone();
        self.fill(&mut state, &wheres);
    }

    /// The spare, when it was started for `at`. Another is started for `at`
    /// when a spare is wanted there, else where one is wanted.
    pub async fn take(&self, at: &Where) -> Option<ClaudeProcess> {
        let place = at.place();
        let next = {
            let mut state = self.state();
            let next = state.next.take();
            state.stopped = false;
            let here = state.wanted.iter().any(|wanted| wanted.place() == place);
            let wheres = if here {
                vec![at.clone()]
            } else {
                state.wanted.clone()
            };
            self.fill(&mut state, &wheres);
            next
        }?;
        if next.place != place {
            kill_spare(next.process);
            return None;
        }
        let process = next.process.await?;
        if process.running() {
            return Some(process);
        }
        process.kill().await;
        None
    }

    pub fn dispose(&self) {
        self.warm(Vec::new());
    }

    /// The spare's process once it has started, so a caller can wait for
    /// it to exit.
    pub fn process(&self) -> Option<BoxFuture<'static, Option<ClaudeProcess>>> {
        self.state()
            .next
            .as_ref()
            .map(|next| next.process.clone().boxed())
    }

    /// Keeps the spare when it fits one of `wheres`, else starts one for the
    /// first; with none, keeps none.
    fn fill(&self, state: &mut SpareState, wheres: &[Where]) {
        if let Some(current) = &state.next
            && wheres.iter().any(|at| at.place() == current.place)
        {
            return;
        }
        if let Some(current) = state.next.take() {
            kill_spare(current.process);
        }
        let Some(first) = wheres.first() else {
            return;
        };
        if state.stopped {
            return;
        }
        let launch = Launch {
            cwd: first.cwd.clone(),
            model: first.model.clone(),
            session_id: Some((self.inner.new_id)()),
            tools: Some(self.inner.tools.clone()),
            ..Default::default()
        };
        let spawn = first.spawn.clone();
        let process: SpareProcess = async move {
            match ClaudeProcess::spawn(&spawn, &launch).await {
                Ok(process) => Some(process),
                Err(error) => {
                    tracing::warn!(%error, "Claude could not start a spare session");
                    None
                }
            }
        }
        .boxed()
        .shared();
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        state.next = Some(Next {
            id,
            place: first.place(),
            process: process.clone(),
        });
        let this = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            if let Some(process) = process.await {
                process.exited().await;
            }
            let Some(inner) = this.upgrade() else {
                return;
            };
            let spare = Spare { inner };
            let mut state = spare.state();
            if state.next.as_ref().is_some_and(|next| next.id == id) {
                state.next = None;
                state.stopped = true;
            }
        });
    }
}

/// Sessions saved in a JSON file, by agent and then conversation: what
/// buzz-app keeps in local storage under `buzz.claude-code.sessions.v1`.
pub struct FileSessions {
    path: PathBuf,
    agent: String,
}

type AllSessions = BTreeMap<String, BTreeMap<String, SavedSession>>;

/// A sessions file, read once and then kept in memory: every message
/// reads and saves its conversation's session.
struct Loaded {
    all: AllSessions,
    /// Changed since the last write began.
    dirty: bool,
    /// A writer thread is running; it writes again while `dirty`.
    writing: bool,
}

/// Agents of one app share each file.
static FILES: Mutex<BTreeMap<PathBuf, Loaded>> = Mutex::new(BTreeMap::new());

impl FileSessions {
    pub fn new(path: PathBuf, agent: &str) -> Self {
        Self {
            path,
            agent: agent.into(),
        }
    }

    /// `<data dir>/buzz-gpui/claude-code-sessions.json`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::data_dir().map(|dir| dir.join("buzz-gpui").join("claude-code-sessions.json"))
    }

    fn read_all(path: &std::path::Path) -> AllSessions {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| {
                let object = value.as_object()?;
                Some(
                    object
                        .iter()
                        .map(|(agent, sessions)| {
                            let sessions = sessions
                                .as_object()
                                .map(|sessions| {
                                    sessions
                                        .iter()
                                        .filter_map(|(key, saved)| {
                                            serde_json::from_value(saved.clone())
                                                .ok()
                                                .map(|saved| (key.clone(), saved))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            (agent.clone(), sessions)
                        })
                        .collect(),
                )
            })
            .unwrap_or_default()
    }

    /// This file's sessions, read from disk the first time.
    fn with<T>(&self, use_them: impl FnOnce(&mut Loaded) -> T) -> T {
        let mut files = FILES.lock().unwrap_or_else(PoisonError::into_inner);
        let loaded = files.entry(self.path.clone()).or_insert_with(|| Loaded {
            all: Self::read_all(&self.path),
            dirty: false,
            writing: false,
        });
        use_them(loaded)
    }

    fn update(&self, change: impl FnOnce(&mut BTreeMap<String, SavedSession>)) {
        let start = self.with(|loaded| {
            let sessions = loaded.all.entry(self.agent.clone()).or_default();
            change(sessions);
            if sessions.len() > SAVED_LIMIT {
                let mut kept: Vec<(String, SavedSession)> =
                    std::mem::take(sessions).into_iter().collect();
                kept.sort_by_key(|(_, saved)| std::cmp::Reverse(saved.at));
                kept.truncate(SAVED_LIMIT);
                *sessions = kept.into_iter().collect();
            }
            loaded.dirty = true;
            !std::mem::replace(&mut loaded.writing, true)
        });
        if start {
            let path = self.path.clone();
            let spawned = std::thread::Builder::new()
                .name("claude-sessions".into())
                .spawn(move || write_sessions(path));
            if let Err(error) = spawned {
                tracing::warn!(%error, "Claude sessions were not saved");
                self.with(|loaded| loaded.writing = false);
            }
        }
    }
}

/// Writes `path` until it holds the newest sessions, off the caller's
/// thread.
fn write_sessions(path: PathBuf) {
    loop {
        let bytes = {
            let mut files = FILES.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(loaded) = files.get_mut(&path) else {
                return;
            };
            if !loaded.dirty {
                loaded.writing = false;
                return;
            }
            loaded.dirty = false;
            serde_json::to_vec(&loaded.all)
        };
        let written = bytes
            .map_err(std::io::Error::from)
            .and_then(|bytes| crate::atomic_file::write(&path, &bytes));
        if let Err(error) = written {
            tracing::warn!(%error, "Claude sessions were not saved");
        }
    }
}

impl SessionStore for FileSessions {
    fn get(&self, key: &str) -> Option<SavedSession> {
        self.with(|loaded| loaded.all.get(&self.agent)?.get(key).cloned())
    }
    fn set(&self, key: &str, session: SavedSession) {
        self.update(|sessions| {
            sessions.insert(key.into(), session);
        });
    }
    fn delete(&self, key: &str) {
        self.update(|sessions| {
            sessions.remove(key);
        });
    }
    fn list(&self) -> Vec<(String, SavedSession)> {
        let mut saved: Vec<(String, SavedSession)> = self.with(|loaded| {
            loaded
                .all
                .get(&self.agent)
                .map(|sessions| sessions.clone().into_iter().collect())
                .unwrap_or_default()
        });
        saved.sort_by_key(|(_, session)| std::cmp::Reverse(session.at));
        saved
    }
}

#[cfg(test)]
mod tests;
