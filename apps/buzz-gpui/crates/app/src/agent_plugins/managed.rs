//! Buzz's own copy of an agent CLI on a host, kept on the latest release:
//! buzz-app's `claude-code/setup.ts` and `codex/setup.ts`, which mirror
//! each other, shared here. The copy lives apart from any the user
//! installed and shares their sign-in and settings. Each step runs as one
//! of the type's declared processes, which the host runs through the
//! copy's script (`crates/host/src/processes/*.sh`).

use crate::agent_plugins::process::{Process, Spawn, SpawnOptions, Spawned, Spawner, collect};
use crate::agents2::Agents2;
use crate::hosts::HostConnection;
use crate::runtime::Runtime;
use buzz_host::api::ProcessOutput;
use futures::future::BoxFuture;
use gpui_kit::{App, Entity, EntityId};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Missing,
    /// Older than Buzz drives; Install updates it.
    Outdated {
        version: String,
    },
    SignedOut {
        version: String,
    },
    Ready {
        version: String,
        account: Option<String>,
    },
    Error {
        message: String,
    },
}

impl Status {
    pub fn version(&self) -> Option<&str> {
        match self {
            Status::Outdated { version }
            | Status::SignedOut { version }
            | Status::Ready { version, .. } => Some(version),
            Status::Missing | Status::Error { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Install,
    Login,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupState {
    pub status: Option<Status>,
    pub checking: bool,
    pub running: Option<Step>,
    /// The running or last step's output.
    pub output: String,
    /// Why the latest release is not the one in use, after it failed its
    /// test.
    pub notice: Option<String>,
}

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const OUTPUT_LIMIT: usize = 20_000;
/// How often each copy looks for a new release.
pub const UPDATE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// Fetches a URL: its status and body.
pub type Fetch =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<(u16, String), String>> + Send + Sync>;

/// A newest release, as its publisher names it.
pub struct Release {
    pub version: String,
    /// What else the release listing said, for [`Tool::checksum`].
    pub data: Value,
}

/// What differs between Claude's copy and Codex's.
pub trait Tool: Send + Sync + 'static {
    /// "Claude" or "Codex".
    fn name(&self) -> &'static str;
    /// Where releases come from, as messages name it.
    fn origin(&self) -> &'static str;
    /// The variable that names a release to run before it is in use.
    fn pin(&self) -> &'static str;
    fn env(&self) -> BTreeMap<String, Option<String>>;
    /// The oldest release Buzz drives.
    fn min_version(&self) -> Option<&'static str> {
        None
    }
    fn check(&self, spawn: Spawner) -> BoxFuture<'static, Status>;
    /// The sign-in process and its arguments.
    fn login(&self) -> (&'static str, Vec<String>);
    /// The release's name for the host's `uname -sm`.
    fn target(&self, uname: &str) -> Option<&'static str>;
    fn latest(&self, fetch: Fetch) -> BoxFuture<'static, Result<Release, String>>;
    /// The release's published SHA-256 for `target`.
    fn checksum(
        &self,
        fetch: Fetch,
        release: &Release,
        target: &str,
    ) -> BoxFuture<'static, Result<Option<String>, String>>;
    /// A downloaded release must start and answer the protocol Buzz drives
    /// it with, listing its models without a turn, before any session uses
    /// it. `spawn` runs that release.
    fn test(&self, spawn: Spawner, version: String) -> BoxFuture<'static, Result<(), String>>;
}

/// GETs `https://` URLs for release listings.
pub fn http() -> Fetch {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    const LIMIT: usize = 4 * 1024 * 1024;
    Arc::new(|url| {
        let client = CLIENT
            .get_or_init(|| {
                reqwest::Client::builder()
                    .https_only(true)
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(Duration::from_secs(60))
                    .build()
                    .expect("build the HTTP client")
            })
            .clone();
        Box::pin(async move {
            let mut response = client
                .get(&url)
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let status = response.status().as_u16();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
                if body.len() + chunk.len() > LIMIT {
                    return Err("The release listing is too large".into());
                }
                body.extend_from_slice(&chunk);
            }
            Ok((status, String::from_utf8_lossy(&body).into_owned()))
        })
    })
}

/// A release listing's body, or why `origin` did not give it.
pub async fn fetch_text(fetch: &Fetch, origin: &str, url: String) -> Result<String, String> {
    let (status, body) = fetch(url).await?;
    if status != 200 {
        return Err(format!("{origin} answered {status}"));
    }
    Ok(body)
}

/// Runs a short process and returns its exit code and output.
pub async fn run_short(
    spawn: &Spawner,
    id: &str,
    args: &[&str],
    env: BTreeMap<String, Option<String>>,
) -> Result<(Option<i32>, String, String), String> {
    let mut spawned = spawn
        .spawn(
            id,
            SpawnOptions {
                args: args.iter().map(|arg| arg.to_string()).collect(),
                cwd: None,
                env,
            },
        )
        .await?;
    let process = spawned.process.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(CHECK_TIMEOUT).await;
        let _ = process.kill().await;
    });
    let result = collect(&mut spawned.output).await;
    timer.abort();
    Ok(result)
}

/// Orders `a.b.c` versions.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |version: &str| -> Vec<u64> {
        version
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    let (left, right) = (parts(a), parts(b));
    (0..3)
        .map(|i| left.get(i).unwrap_or(&0).cmp(right.get(i).unwrap_or(&0)))
        .find(|order| order.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

fn release_name(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Runs the type's processes as the given release rather than the one in
/// use.
struct Pinned {
    spawn: Spawner,
    var: &'static str,
    version: String,
}

impl Spawn for Pinned {
    fn spawn(
        &self,
        id: &str,
        mut options: SpawnOptions,
    ) -> BoxFuture<'static, Result<Spawned, String>> {
        options
            .env
            .insert(self.var.into(), Some(self.version.clone()));
        self.spawn.spawn(id, options)
    }

    fn host(&self) -> usize {
        self.spawn.host()
    }
}

/// The one view of a tool's copy on a host. Cheap to clone.
#[derive(Clone)]
pub struct Setup {
    inner: Arc<Inner>,
}

struct Inner {
    tool: Arc<dyn Tool>,
    spawn: Option<Spawner>,
    fetch: Fetch,
    state: watch::Sender<SetupState>,
    process: Mutex<Option<Process>>,
    /// Cancel was asked for before the running step's process was returned.
    cancelled: AtomicBool,
    /// Releases that failed their test, not downloaded again until restart.
    refused: Mutex<HashSet<String>>,
    /// Looking for the copy or downloading one, which deliveries wait for
    /// while no copy has been found: the latest such work.
    holding: watch::Sender<Option<u64>>,
    holds: AtomicU64,
}

/// Work deliveries wait on until it ends, unless later work took over.
struct Hold(Arc<Inner>, u64);

impl Drop for Hold {
    fn drop(&mut self) {
        let id = self.1;
        self.0.holding.send_if_modified(|holding| {
            let ours = *holding == Some(id);
            if ours {
                *holding = None;
            }
            ours
        });
    }
}

impl Setup {
    pub fn new(tool: Arc<dyn Tool>, spawn: Option<Spawner>, fetch: Fetch) -> Self {
        Self {
            inner: Arc::new(Inner {
                tool,
                spawn,
                fetch,
                state: watch::Sender::new(SetupState::default()),
                process: Mutex::default(),
                cancelled: AtomicBool::new(false),
                refused: Mutex::default(),
                holding: watch::Sender::new(None),
                holds: AtomicU64::new(0),
            }),
        }
    }

    pub fn snapshot(&self) -> SetupState {
        self.inner.state.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<SetupState> {
        self.inner.state.subscribe()
    }
    fn set(&self, change: impl FnOnce(&mut SetupState)) {
        self.inner.state.send_modify(change);
    }
    fn process(&self) -> std::sync::MutexGuard<'_, Option<Process>> {
        self.inner
            .process
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
    fn refused(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.inner
            .refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
    fn log(&self, text: &str) {
        self.set(|state| {
            state.output.push_str(text);
            let length = state.output.chars().count();
            if length > OUTPUT_LIMIT {
                state.output = state.output.chars().skip(length - OUTPUT_LIMIT).collect();
            }
        });
    }
    fn hold(&self) -> Hold {
        let id = self.inner.holds.fetch_add(1, Ordering::Relaxed) + 1;
        self.inner.holding.send_replace(Some(id));
        Hold(self.inner.clone(), id)
    }

    /// Looks again, or joins the look in progress, and returns what it
    /// found.
    pub async fn check(&self) -> Status {
        let Some(spawn) = self.inner.spawn.clone() else {
            let status = Status::Error {
                message: format!(
                    "{} agents run only in the desktop app",
                    self.inner.tool.name()
                ),
            };
            self.set(|state| state.status = Some(status.clone()));
            return status;
        };
        let mut already = false;
        self.set(|state| {
            already = state.checking;
            state.checking = true;
        });
        if already {
            let mut changes = self.subscribe();
            let _ = changes.wait_for(|state| !state.checking).await;
            return self.snapshot().status.unwrap_or(Status::Missing);
        }
        let status = self.inner.tool.check(spawn).await;
        self.set(|state| {
            state.status = Some(status.clone());
            state.checking = false;
        });
        status
    }

    /// Moves the copy to the latest release. Unless `needed`, a host
    /// without a copy is left without one: the download is large. Held
    /// from the first check, before the future is first polled: until it
    /// answers, there may be no copy.
    pub fn update(&self, needed: bool) -> impl Future<Output = ()> + Send + 'static {
        let this = self.clone();
        let hold =
            (self.inner.spawn.is_some() && self.snapshot().running.is_none()).then(|| self.hold());
        async move {
            let (Some(_hold), Some(spawn)) = (hold, this.inner.spawn.clone()) else {
                return;
            };
            let status = this.inner.tool.check(spawn).await;
            this.set(|state| state.status = Some(status.clone()));
            if !needed && status == Status::Missing {
                return;
            }
            this.perform(Step::Install, false).await;
        }
    }

    /// Holds a delivery while Buzz looks for its copy and downloads a
    /// first one, so a mention sent meanwhile is answered rather than lost
    /// to a missing copy. Once a copy is found, updating it holds nothing.
    /// Must run inside tokio.
    pub async fn installed(&self) {
        let found = |state: &SetupState| {
            state
                .status
                .as_ref()
                .is_some_and(|status| *status != Status::Missing)
        };
        // After a first download failed, a mention tries it again.
        if self.inner.holding.borrow().is_none() && self.snapshot().status == Some(Status::Missing)
        {
            tokio::spawn(self.update(true));
        }
        let mut holds = self.inner.holding.subscribe();
        let mut states = self.subscribe();
        loop {
            if holds.borrow_and_update().is_none() || found(&states.borrow_and_update()) {
                return;
            }
            tokio::select! {
                changed = holds.changed() => if changed.is_err() { return },
                changed = states.changed() => if changed.is_err() { return },
            }
        }
    }

    /// Installs or updates the copy, or runs sign-in; then checks again.
    /// Asked for by the person, so it also retries a release that failed
    /// its test, and shows only this step's output.
    pub async fn run(&self, step: Step) {
        self.perform(step, true).await;
    }

    async fn perform(&self, step: Step, asked: bool) {
        let Some(spawn) = self.inner.spawn.clone() else {
            return;
        };
        let mut already = false;
        self.set(|state| {
            already = state.running.is_some();
            if !already {
                state.running = Some(step);
                if asked {
                    state.output.clear();
                }
            }
        });
        if already {
            return;
        }
        let _hold = (step == Step::Install).then(|| self.hold());
        self.inner.cancelled.store(false, Ordering::SeqCst);
        if asked {
            self.refused().clear();
        }
        let result = match step {
            Step::Install => self.install(&spawn).await,
            Step::Login => {
                let (id, args) = self.inner.tool.login();
                self.step(&spawn, id, args, "Sign-in").await
            }
        };
        if let Err(error) = result {
            self.log(&format!("\n{error}"));
        }
        *self.process() = None;
        self.set(|state| state.running = None);
        self.check().await;
    }

    /// Runs one process with its output shown; fails unless it succeeds.
    async fn step(
        &self,
        spawn: &Spawner,
        id: &str,
        args: Vec<String>,
        label: &str,
    ) -> Result<(), String> {
        let stopped = || format!("{label} ended (stopped).");
        if self.inner.cancelled.load(Ordering::SeqCst) {
            return Err(stopped());
        }
        let mut spawned = spawn
            .spawn(
                id,
                SpawnOptions {
                    args,
                    cwd: None,
                    env: self.inner.tool.env(),
                },
            )
            .await?;
        *self.process() = Some(spawned.process.clone());
        if self.inner.cancelled.load(Ordering::SeqCst) {
            let _ = spawned.process.kill().await;
        }
        let mut code = None;
        while let Some(item) = spawned.output.recv().await {
            match item {
                ProcessOutput::Stdout(text) | ProcessOutput::Stderr(text) => self.log(&text),
                ProcessOutput::Exit(exit) => {
                    code = exit;
                    break;
                }
            }
        }
        *self.process() = None;
        match code {
            Some(0) => Ok(()),
            Some(code) => Err(format!("{label} ended ({code}).")),
            None => Err(stopped()),
        }
    }

    /// Downloads the latest release, checks it against its published
    /// checksum, tests it, and only then makes it the one new sessions
    /// start.
    async fn install(&self, spawn: &Spawner) -> Result<(), String> {
        let tool = self.inner.tool.clone();
        let name = tool.name();
        let status = tool.check(spawn.clone()).await;
        let current = status.version().map(str::to_string);
        let release = tool.latest(self.inner.fetch.clone()).await?;
        let latest = release.version.clone();
        if !release_name(&latest) {
            return Err(format!("{} did not name a release", tool.origin()));
        }
        if current.as_deref() == Some(latest.as_str())
            || self.refused().contains(&latest)
            || self.inner.cancelled.load(Ordering::SeqCst)
        {
            return Ok(());
        }
        if let Some(min) = tool.min_version()
            && compare_versions(&latest, min).is_lt()
        {
            return Err(format!("{name} {latest} is older than Buzz needs ({min})."));
        }
        let (_, uname, _) = run_short(spawn, "platform", &[], BTreeMap::new()).await?;
        let target = tool
            .target(uname.trim())
            .ok_or_else(|| format!("{name} is not available for this computer"))?;
        let checksum = tool
            .checksum(self.inner.fetch.clone(), &release, target)
            .await?
            .filter(|sum| {
                sum.len() == 64 && sum.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            })
            .ok_or_else(|| format!("{name} {latest} lists no checksum for {target}"))?;
        self.log(&format!("Downloading {name} {latest}…\n"));
        self.step(
            spawn,
            "install",
            vec![latest.clone(), target.into(), checksum],
            "Download",
        )
        .await?;
        self.log(&format!("Testing {name} {latest}…\n"));
        let pinned: Spawner = Arc::new(Pinned {
            spawn: spawn.clone(),
            var: tool.pin(),
            version: latest.clone(),
        });
        if let Err(error) = tool.test(pinned, latest.clone()).await {
            self.refused().insert(latest.clone());
            let reason = format!("{name} {latest} failed its test: {error}");
            let notice = match &current {
                Some(current) => format!("{reason}. Buzz keeps using {current}."),
                None => reason.clone(),
            };
            self.set(|state| state.notice = Some(notice));
            return Err(reason);
        }
        self.step(spawn, "use", vec![latest.clone()], "Switching")
            .await?;
        self.set(|state| state.notice = None);
        self.log(&format!("{name} {latest} is installed.\n"));
        Ok(())
    }

    /// Sends a line to the running step, such as a sign-in code.
    pub async fn answer(&self, text: &str) {
        let process = self.process().clone();
        if let Some(process) = process {
            let _ = process.write(format!("{text}\n")).await;
        }
    }

    pub async fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let process = self.process().clone();
        if let Some(process) = process {
            let _ = process.kill().await;
        }
    }
}

/// Makes a type's setup for a host: its spawner, if connected, and
/// whether it is remote.
pub type Make = fn(Option<Spawner>, bool) -> Setup;

/// A type's copy on each host, kept up to date.
pub struct Copies {
    owner: &'static str,
    make: Make,
    setups: HashMap<EntityId, Setup>,
    /// Whether an agent of the type runs on each host seen since the
    /// agents loaded.
    needed: HashMap<EntityId, bool>,
}

impl Copies {
    pub fn new(owner: &'static str, make: Make) -> Self {
        Self {
            owner,
            make,
            setups: HashMap::new(),
            needed: HashMap::new(),
        }
    }

    pub fn setup(&mut self, host: &Entity<HostConnection>, cx: &App) -> Setup {
        let (owner, make) = (self.owner, self.make);
        self.setups
            .entry(host.entity_id())
            .or_insert_with(|| {
                let host = host.read(cx);
                make(Some(host.spawner(owner)), host.remote.is_some())
            })
            .clone()
    }

    /// The setup on the host that holds `pubkey`.
    pub fn setup_of(&mut self, pubkey: &str, cx: &App) -> Setup {
        let host = Agents2::global(cx).and_then(|agents| agents.read(cx).host_of(pubkey));
        match host {
            Some(host) => self.setup(&host, cx),
            None => (self.make)(None, false),
        }
    }

    /// Once the agents have loaded, and when a host's first agent of type
    /// `key` runs, its copy is brought up to date; that is also when a
    /// first copy is downloaded. Call while the agents are not loading.
    pub fn sync(&mut self, agents: &Agents2, key: &str, cx: &App) {
        let hosts = agents.hosts(cx);
        let mut needed: HashMap<EntityId, bool> =
            hosts.iter().map(|host| (host.entity_id(), false)).collect();
        for agent in agents
            .running()
            .iter()
            .filter(|agent| agent.agent_type == key)
        {
            if let Some(host) = agents.host_of(&agent.pubkey)
                && let Some(running) = needed.get_mut(&host.entity_id())
            {
                *running = true;
            }
        }
        let runtime = Runtime::handle(cx);
        for host in &hosts {
            let now = needed[&host.entity_id()];
            let was = self.needed.get(&host.entity_id()).copied();
            if was.is_none() || (now && was == Some(false)) {
                runtime.spawn(self.setup(host, cx).update(now));
            }
        }
        self.needed = needed;
    }

    /// Looks for a new release on every host seen.
    pub fn refresh(&self, cx: &App) {
        let runtime = Runtime::handle(cx);
        for (host, needed) in &self.needed {
            if let Some(setup) = self.setups.get(host) {
                runtime.spawn(setup.update(*needed));
            }
        }
    }
}

#[cfg(test)]
pub mod testing;
#[cfg(test)]
mod tests;
