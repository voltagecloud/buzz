//! The Agents2 service (buzz-app's `features/agents2/service.ts`).
//!
//! The agent list is the union of every connected host's saved Agents2
//! identities; `records` only adds local settings to them. Every change is
//! made on the host first and then read back, so the list has one source.
//! The running agents are those the signed-in owner made for any joined
//! community, selected or not; the visible ones are the selected
//! community's. Each saved agent has one runner for as long as its identity
//! exists: its own community's matching live events and due timers queue
//! there and run one at a time through its type's `run`. Every GUI on the
//! agent's host, and every copy of the app sharing it, hears the same
//! events; only the one the host lets hold the agent runs them.

use super::attention::{
    self, AttentionObject, AttentionValue, CHAT_KINDS, EventData, Predicate, TimerState,
    addressed_to, compile_filter, tags_agent, timer_spent, timer_state, watch_matches,
};
use super::objects::{CLASSIFIER, ClassifierAvailability};
use super::registry::{self, AgentType, CarryKey, Models, RegisteredType, RetypePreview};
use super::runtime::{
    AgentAttention, AgentHandle, AttentionCall, Bounded, CancelSource, Cancellation,
    ClassifierSkip, Delivery, HostLink, Trigger, UNSUPPORTED,
};
use super::store::{
    self, AgentRecord, FileStorage, Schedule, Storage, read_records, read_teams, set_attention,
    write_records, write_teams,
};
use super::teams::{self, Team, TeamChange};
use super::working::{self, Working};
use super::{Agent, AgentChange, Made};
use super::{presence, typing};
use crate::agent_activity::local::{self, Begin, LocalActivity, WorkHandle, WorkOutcome};
use crate::community::{AppState, Community, IdentityState};
use crate::hosts::{HostConnection, Hosts};
use crate::runtime::Runtime;
use buzz_host::api::{AppAgentRequest, AppAgentResponse, AppAgentSummary, PortableAgent};
use buzz_host_rpc::ConnectionState;
use buzz_identity::Identity;
use buzz_relay_client::{LiveBatch, RelayOrigin, Session, SessionStatus};
use futures::FutureExt as _;
use gpui_kit::{App, AppContext, Context, Entity, EntityId, Global, Subscription, Task};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const QUEUE_LIMIT: usize = 32;
/// Runs per agent per minute; bounds two agents that answer each other.
pub(crate) const RATE_LIMIT: u32 = 60;
pub(crate) const RATE_WINDOW: Duration = Duration::from_secs(60);
const TIMER_TICK: Duration = Duration::from_secs(5);
/// How often a failed connection to an unselected community is tried again.
pub(crate) const RETRY: Duration = Duration::from_secs(30);
/// Bounds one cleanup attempt.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(60);
/// What setting a picture on a host from before pictures answers.
pub const PICTURES_UNSUPPORTED: &str = "Update the agent's host to change its picture.";
/// What a host from before provider changes answers a change of type.
pub const TYPES_UNSUPPORTED: &str = "Update the agent's host to change its provider.";

/// Whether the agent list could be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// No agent host is available.
    Unavailable,
    Loading,
    Ready,
    Error(String),
}

/// One saved identity and the host that holds its key.
#[derive(Clone)]
pub(crate) struct Saved {
    pub summary: AppAgentSummary,
    pub host: Entity<HostConnection>,
}

#[derive(Clone, Debug, PartialEq)]
enum ListState {
    Loading,
    Ready,
    Error(String),
    /// A host from before Agents2.
    Unsupported,
}

struct HostList {
    host: Entity<HostConnection>,
    state: ListState,
    agents: Vec<AppAgentSummary>,
    /// Counts list requests, so only the latest one is applied.
    turn: u64,
    connected: bool,
    _observe: Subscription,
}

/// A community's session while it is ready.
struct Binding {
    /// The relay URL new agents are made for.
    destination: String,
    viewer: String,
    identity: Arc<Identity>,
    session: Arc<Session>,
    live: bool,
    /// Ends work started for this binding when it is replaced.
    generation: u64,
    _events: Task<()>,
}

/// A community the viewer has an agent in, or the selected one, and its
/// binding while its session is ready.
struct Bound {
    community: Entity<Community>,
    binding: Option<Binding>,
    /// When its failed connection was last tried again.
    retried: Option<Instant>,
    _observe: Subscription,
}

/// Each bound community's `https://host[:port]` origin, and the viewer its
/// session is ready for, if it is.
pub(crate) type Sessions = HashMap<String, Option<String>>;

/// The communities to connect: the selected one, where agents are made,
/// and each one the viewer has an agent in, deleted ones included so their
/// cleanup can finish. `identities` are each saved agent's owner and relay.
pub(crate) fn wanted<'a>(
    viewer: Option<&str>,
    selected: Option<&str>,
    identities: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> BTreeSet<String> {
    let Some(viewer) = viewer else {
        return BTreeSet::new();
    };
    selected
        .map(str::to_owned)
        .into_iter()
        .chain(
            identities
                .into_iter()
                .filter(|(owner, _)| *owner == viewer)
                .filter_map(|(_, relay)| http_origin(relay)),
        )
        .collect()
}

/// Whether the agent stays running: it is the viewer's, and its community
/// is joined and bound. A reconnect does not stop it.
pub(crate) fn alive(owner: &str, relay: &str, viewer: Option<&str>, sessions: &Sessions) -> bool {
    viewer == Some(owner) && http_origin(relay).is_some_and(|origin| sessions.contains_key(&origin))
}

/// Whether the agent's community session is ready to run it in.
pub(crate) fn ready(owner: &str, relay: &str, sessions: &Sessions) -> bool {
    http_origin(relay)
        .and_then(|origin| sessions.get(&origin))
        .is_some_and(|viewer| viewer.as_deref() == Some(owner))
}

/// Whether the Agents2 page shows a running agent: it is in the selected
/// community, and that community's session is ready.
pub(crate) fn shown(owner: &str, relay: &str, selected: Option<&str>, sessions: &Sessions) -> bool {
    selected.is_some() && http_origin(relay).as_deref() == selected && ready(owner, relay, sessions)
}

/// Whether a running agent hears a live event from the community at
/// `origin`: only its own community's, while it is ready.
pub(crate) fn hears(owner: &str, relay: &str, origin: &str, sessions: &Sessions) -> bool {
    http_origin(relay).as_deref() == Some(origin) && ready(owner, relay, sessions)
}

/// Whether a failed community connection is tried again now: only one
/// that is not selected, since nobody is there to press its Retry, and at
/// most every [`RETRY`].
pub(crate) fn retry_due(
    selected: bool,
    failed: bool,
    retried: Option<Instant>,
    now: Instant,
) -> bool {
    !selected && failed && retried.is_none_or(|at| now.duration_since(at) >= RETRY)
}

#[derive(Clone)]
struct Job {
    trigger: Trigger,
    channel: Option<String>,
    /// A mention's local work row, once the runner holds the agent.
    work: Option<WorkHandle>,
    /// For a watch or timer: the object and attention epoch it was
    /// matched or found due under. It runs only while both are still
    /// current, so a watch or timer edited, disabled, rearmed, replaced or
    /// turned off and on again since then does not run it.
    matched: Option<(AttentionObject, u64)>,
}

pub(crate) struct CompiledWatch {
    slug: String,
    watch: attention::EventWatch,
    interest: Option<attention::Interest>,
    classifier: Option<ClassifierSkip>,
    filter: Option<Predicate>,
}

/// Until a classifier is wired in, every classified watch passes
/// unchecked.
const UNAVAILABLE: &str = "no classifier is available on this device";

/// The watches compiled for one attention set.
type Compiled = (
    Arc<BTreeMap<String, AttentionObject>>,
    Arc<Vec<CompiledWatch>>,
);

/// One agent's runtime, for as long as its identity exists. Edits do not
/// replace it: each job reads the agent as it is when the job starts.
struct Runner {
    /// The registration whose `run` it calls; a new one cancels the run in
    /// flight.
    kind: Option<RegisteredType>,
    /// Fires when the agent is deleted or its type replaced.
    lifetime: Arc<CancelSource>,
    lifetime_signal: Cancellation,
    queue: VecDeque<Job>,
    running: bool,
    window: Window,
    /// Events seen, and events it wrote (so reactions to them reach no
    /// watch).
    seen: Bounded,
    wrote: Arc<Mutex<Bounded>>,
    compiled: Option<Compiled>,
    /// Tells a replaced runner's drain loop to stop.
    serial: u64,
    /// Whether the host may hold the agent for this GUI; true at first, so
    /// an agent that starts out of view is let go of once.
    may_hold: bool,
    /// Whether a claim has been answered, so `may_hold` is known rather
    /// than assumed.
    asked: bool,
    /// When the run in progress started, Unix ms.
    working_since: Option<u64>,
    /// The last run's failure: when it ended, Unix ms, and why.
    failed: Option<(u64, String)>,
    /// The host lets this GUI hold the agent, so its mentions show here
    /// as local work.
    activity_claimed: bool,
    /// Advances each time attention is seen off, which voids queued watch
    /// work.
    epoch: u64,
    /// Runs still counted as working, work they handed off included,
    /// which a change of type waits out.
    working: Arc<AtomicUsize>,
    /// A change of type holds the idle runner: no run starts meanwhile,
    /// and its queue resumes after.
    transitioning: bool,
    /// Its typing pulses, one per conversation it is working in.
    typing: typing::Pulses,
}

/// What a full queue says on the mention it could not take.
const BUSY: &str = "Agent is busy and could not queue this request. Try again when it finishes.";

/// Starts showing a queued mention as received, in its community.
fn show_work(local: &LocalActivity, scope: &str, agent: &str, job: &mut Job) {
    if job.work.is_some() {
        return;
    }
    let Trigger::Mention { event } = &job.trigger else {
        return;
    };
    let Some(channel) = job
        .channel
        .clone()
        .or_else(|| event.tag("h").map(str::to_owned))
    else {
        return;
    };
    job.work = Some(local.begin(
        Begin {
            scope: scope.to_owned(),
            agent: agent.to_owned(),
            channel_id: channel,
            message_id: event.id.clone(),
            thread_root_id: working::thread_root(event),
        },
        now_ms(),
    ));
}

/// The local work scope of an agent's community.
fn work_scope(agent: &Agent) -> Option<String> {
    Some(local::scope(&http_origin(&agent.relay)?, &agent.owner))
}

/// One saved identity as the rest of the app knows the viewer's agents
/// (`LocalAgent`): for names, mentions and typing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAgent {
    pub pubkey: String,
    pub name: String,
    /// The canonical `wss://` community origin it was created for.
    pub relay: String,
    pub owner: String,
}

/// An identity an earlier agent system saved on one of the hosts, which
/// Agents can take over there.
#[derive(Clone)]
pub struct Portable {
    pub agent: PortableAgent,
    pub host: Entity<HostConnection>,
}

/// What every host could bring over (`PortableAgents`).
#[derive(Clone, Default)]
pub struct Portables {
    pub agents: Vec<Portable>,
    /// A store that could not be read; the others are still listed.
    pub warnings: Vec<String>,
}

/// What a runner is doing, for Mission control.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunState {
    /// When the run in progress started, Unix ms.
    pub working_since: Option<u64>,
    /// This app holds the agent; another app on its host runs it otherwise.
    pub held: bool,
    /// Its community is ready to run it in.
    pub ready: bool,
    /// The last run's failure, until a run succeeds: when it ended, Unix
    /// ms, and why.
    pub failed: Option<(u64, String)>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// The agent and runnable type a drain step saw, to tell whether either
/// changed while it waited on the host.
#[derive(Clone)]
struct Seen {
    agent: Option<Agent>,
    kind: Option<RegisteredType>,
}

/// Why a local agent cannot become a hosted type's (`retype`).
const HOSTED_RETYPE: &str = "A hosted provider cannot take over a local agent key";
impl PartialEq for Seen {
    fn eq(&self, other: &Self) -> bool {
        self.agent == other.agent
            && match (&self.kind, &other.kind) {
                (Some(a), Some(b)) => Arc::ptr_eq(&a.kind, &b.kind),
                (None, None) => true,
                _ => false,
            }
    }
}

/// What a runner does next.
enum Step {
    Idle,
    /// It cannot run here: let another GUI or copy of the app have it.
    Release(HostLink, Seen),
    /// Ask the host to hold it before its next job.
    Claim(HostLink, Seen),
}

/// A job started, or why none did.
enum Start {
    Idle,
    /// The agent or its type changed while the claim was out.
    Changed,
    /// The type runs the delivery once this service's update has ended:
    /// a type reads the service (its agent's host) as it starts.
    Run(
        Arc<dyn AgentType>,
        Box<(Delivery, Agent)>,
        Cancellation,
        CancelSource,
    ),
}

/// Whether the host's answer to a claim lets this GUI run the agent. A
/// claim that fails, as on a host from before claims, runs it anyway:
/// answering twice beats not answering at all.
fn claimed(pubkey: &str, answer: Result<AppAgentResponse, String>) -> bool {
    match answer {
        Ok(AppAgentResponse::Claimed(claimed)) => claimed,
        Ok(_) => true,
        Err(error) => {
            if error != UNSUPPORTED {
                tracing::warn!(agent = %pubkey, %error, "agent was not claimed");
            }
            true
        }
    }
}

fn answering(name: &str) -> String {
    format!("{name} is answering; change its type when it finishes")
}

/// Why a change of type cannot start now, if it cannot: the agent is
/// answering (`working` runs, handed-off work included), this app has no
/// runner for it, or another change holds it.
pub(crate) fn retype_refusal(
    name: &str,
    working: usize,
    runs_here: bool,
    transitioning: bool,
) -> Option<String> {
    if working > 0 {
        Some(answering(name))
    } else if !runs_here {
        Some("Change its type on the device that runs it".into())
    } else if transitioning {
        Some(format!("{name}'s provider is already changing"))
    } else {
        None
    }
}

/// The model and effort the triggering message asks of this agent, if
/// any. Only the owner picks them, so nobody else in a channel can run the
/// agent at their level.
pub(crate) fn asked(
    trigger: &Trigger,
    agent: &str,
    owner: &str,
) -> Option<(Option<String>, Option<String>)> {
    let event = match trigger {
        Trigger::Mention { event } | Trigger::Watch { event, .. } => event,
        Trigger::Timer { .. } => return None,
    };
    if event.pubkey != owner {
        return None;
    }
    let tag = |name: &str| {
        event
            .tags
            .iter()
            .find(|tag| {
                tag.first().map(String::as_str) == Some(name)
                    && tag.get(1).map(String::as_str) == Some(agent)
            })
            .and_then(|tag| tag.get(2).cloned())
            .filter(|value| !value.is_empty())
    };
    let model = tag(buzz_relay_client::messages::MODEL_TAG);
    let effort = tag(buzz_relay_client::messages::EFFORT_TAG);
    (model.is_some() || effort.is_some()).then_some((model, effort))
}

/// Of what was asked, only what the type offers: a listed model, and an
/// effort of the model the run uses.
pub(crate) fn picked(
    (model, effort): (Option<String>, Option<String>),
    offered: Option<&Models>,
) -> (Option<String>, Option<String>) {
    let Some(offered) = offered else {
        return (None, None);
    };
    let picked = offered
        .models
        .iter()
        .find(|item| Some(&item.id) == model.as_ref());
    let runs = picked.or_else(|| {
        offered
            .models
            .iter()
            .find(|item| item.id == offered.current)
    });
    let effort = effort.filter(|effort| {
        runs.is_some_and(|runs| runs.efforts.iter().any(|level| level.id == *effort))
    });
    (picked.map(|picked| picked.id.clone()), effort)
}

/// The per-minute admission window.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Window {
    pub start: Option<Instant>,
    pub admitted: u32,
}

/// Whether one more job may queue, counting it if so: at most
/// [`QUEUE_LIMIT`] waiting and [`RATE_LIMIT`] admitted per minute.
pub(crate) fn admit(window: &mut Window, queued: usize, now: Instant) -> bool {
    if window
        .start
        .is_none_or(|start| now.duration_since(start) >= RATE_WINDOW)
    {
        window.start = Some(now);
        window.admitted = 0;
    }
    if queued >= QUEUE_LIMIT || window.admitted >= RATE_LIMIT {
        return false;
    }
    window.admitted += 1;
    true
}

/// What one event wakes an agent of `owner` for. Another of the owner's
/// agents wakes it only by naming it, wherever: were a reply to it enough,
/// two agents replying to each other would never stop. The claimed owner
/// only narrows this; the type verifies it before answering. Addressed
/// events never reach a watch, and only conversation starts a mention run.
pub(crate) fn heard(
    agent: &str,
    owner: &str,
    event: &Arc<EventData>,
    wrote: &dyn Fn(&str) -> bool,
    watches: &[CompiledWatch],
) -> Vec<Trigger> {
    let mention = || {
        vec![Trigger::Mention {
            event: event.clone(),
        }]
    };
    let sibling = event.tags.iter().any(|tag| {
        tag.first().map(String::as_str) == Some("auth")
            && tag.get(1).map(String::as_str) == Some(owner)
    });
    if sibling {
        return if CHAT_KINDS.contains(&event.kind) && tags_agent(event, agent) {
            mention()
        } else {
            Vec::new()
        };
    }
    if addressed_to(event, agent, wrote) {
        return if CHAT_KINDS.contains(&event.kind) {
            mention()
        } else {
            Vec::new()
        };
    }
    watches
        .iter()
        .filter(|compiled| watch_matches(&compiled.watch, event, compiled.filter.as_ref()))
        .map(|compiled| Trigger::Watch {
            event: event.clone(),
            slug: compiled.slug.clone(),
            watch: compiled.watch.clone(),
            interest: compiled.interest.clone(),
            classifier: compiled.classifier.clone(),
        })
        .collect()
}

/// The agent's enabled event watches.
fn compile(attention: &BTreeMap<String, AttentionObject>) -> Vec<CompiledWatch> {
    attention
        .values()
        .filter_map(|object| {
            let AttentionValue::Event(watch) = &object.value else {
                return None;
            };
            if !watch.enabled {
                return None;
            }
            Some(CompiledWatch {
                slug: object.slug.clone(),
                interest: interest(attention, &watch.interest_id),
                classifier: (watch.classifier.is_some()
                    && CLASSIFIER == ClassifierAvailability::Unavailable)
                    .then(|| ClassifierSkip {
                        reason: UNAVAILABLE.into(),
                    }),
                filter: watch
                    .filter
                    .as_deref()
                    .and_then(|source| compile_filter(source).ok()),
                watch: watch.clone(),
            })
        })
        .collect()
}

fn interest(
    attention: &BTreeMap<String, AttentionObject>,
    id: &str,
) -> Option<attention::Interest> {
    match attention
        .get(&attention::interest_slug(id))
        .map(|o| &o.value)
    {
        Some(AttentionValue::Interest(interest)) => Some(interest.clone()),
        _ => None,
    }
}

/// A due timer's slug, value and state once this run counts.
pub(crate) type DueTimer = (String, attention::TimerWatch, TimerState);

/// Spec schedule rules: occurrence k is due at armed_at + k × interval, at
/// most one runs when overdue, then the next is due interval after it ran.
/// A timer with no saved state counts what was already due as used. The
/// longest-waiting timer runs first, so one whose runs outlast its interval
/// cannot hold back the others. Returns the new timer states and the timer
/// to run, if any.
pub(crate) fn due_timer(
    attention: &BTreeMap<String, AttentionObject>,
    prior: &BTreeMap<String, TimerState>,
    now: u64,
) -> (BTreeMap<String, TimerState>, Option<DueTimer>) {
    let mut timers = prior.clone();
    let mut next: Option<(&str, &attention::TimerWatch, TimerState)> = None;
    for object in attention.values() {
        let AttentionValue::Timer(timer) = &object.value else {
            continue;
        };
        if !timer.enabled {
            continue;
        }
        let saved = prior.get(&object.slug);
        let state = timer_state(timer, saved, now);
        if saved != Some(&state) {
            timers.insert(object.slug.clone(), state);
        }
        if now < state.next_due || timer_spent(timer, &state, now) {
            continue;
        }
        if next
            .as_ref()
            .is_none_or(|(_, _, best)| state.next_due < best.next_due)
        {
            next = Some((&object.slug, timer, state));
        }
    }
    let Some((slug, timer, state)) = next else {
        return (timers, None);
    };
    let ran = TimerState {
        used: state.used + 1,
        next_due: now + timer.interval_secs,
        ..state
    };
    timers.insert(slug.to_owned(), ran);
    (timers, Some((slug.to_owned(), timer.clone(), ran)))
}

/// The job as it should run now, or `None` when it should not run: a
/// watch's event is matched again against the agent's current attention,
/// so one turned off, disabled, removed, replaced or changed since it was
/// queued wakes nothing, even if it now matches again.
fn rematched(epoch: u64, agent: &Agent, job: Job) -> Option<Job> {
    let Trigger::Watch { event, slug, .. } = &job.trigger else {
        return Some(job);
    };
    if !agent.attention_enabled {
        return None;
    }
    let (object, matched) = job.matched.as_ref()?;
    if *matched != epoch || agent.attention.get(slug) != Some(object) {
        return None;
    }
    let compiled = compile(&agent.attention).into_iter().find(|compiled| {
        compiled.slug == *slug && watch_matches(&compiled.watch, event, compiled.filter.as_ref())
    })?;
    Some(Job {
        trigger: Trigger::Watch {
            event: event.clone(),
            slug: compiled.slug,
            watch: compiled.watch,
            interest: compiled.interest,
            classifier: compiled.classifier,
        },
        ..job
    })
}

/// Whether `job`, already started, should still run: the rule
/// [`rematched`] applies to a queued job, asked again for work a type
/// queued itself.
fn still_current(epoch: u64, agent: &Agent, job: &Job) -> bool {
    match &job.trigger {
        Trigger::Mention { .. } => true,
        Trigger::Watch { .. } => rematched(epoch, agent, job.clone()).is_some(),
        // A timer occurrence belongs to the object and epoch it fell due
        // under: attention off and on again, or a disable and rearm, voids
        // it.
        Trigger::Timer { slug, .. } => {
            let object = agent.attention.get(slug);
            agent.attention_enabled
                && job
                    .matched
                    .as_ref()
                    .is_some_and(|(matched, at)| *at == epoch && object == Some(matched))
                && matches!(object.map(|o| &o.value), Some(AttentionValue::Timer(timer)) if timer.enabled)
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A new agent's settings: its type's defaults.
fn defaults(pubkey: &str, kind: &RegisteredType) -> Result<AgentRecord, String> {
    let defaults = kind.kind.defaults();
    let mut record = AgentRecord {
        config: defaults.config,
        ..AgentRecord::blank(pubkey)
    };
    let now = now_secs();
    for (slug, value) in &defaults.attention {
        record = set_attention(&record, slug, Some(&value.to_json()), now, None)?;
    }
    Ok(record)
}

/// The http origin of a saved `wss://` relay.
fn http_origin(relay: &str) -> Option<String> {
    RelayOrigin::parse(relay).ok().map(|origin| origin.http())
}

struct Agents2Global(Entity<Agents2>);

impl Global for Agents2Global {}

pub(super) fn init(cx: &mut App) {
    let dir = crate::community::data_dir();
    store::migrate_storage(&dir);
    let storage: Arc<dyn Storage> = Arc::new(FileStorage(
        dir.join(format!("{}.json", store::STORAGE_KEY)),
    ));
    let team_storage: Arc<dyn Storage> =
        Arc::new(FileStorage(dir.join(format!("{}.json", store::TEAMS_KEY))));
    let service = cx.new(|cx| Agents2::new(storage, team_storage, cx));
    cx.set_global(Agents2Global(service));
}

pub struct Agents2 {
    storage: Arc<dyn Storage>,
    records: BTreeMap<String, AgentRecord>,
    team_storage: Arc<dyn Storage>,
    /// The owner's teams on this device, in any community.
    teams: Vec<Team>,
    lists: Vec<(EntityId, HostList)>,
    status: Status,
    /// The selected community's agents, while it is ready.
    agents: Vec<Agent>,
    /// Every agent kept running here, whichever community is selected:
    /// the viewer's agents in each joined community.
    running: Vec<Agent>,
    /// By origin, so each agent listens in its own community, selected or
    /// not.
    bound: HashMap<String, Bound>,
    /// Agents whose community was ready at the last update.
    connected: HashSet<String>,
    generation: u64,
    runners: HashMap<String, Runner>,
    serial: u64,
    /// Deleted agents whose community cleanup is running, and whether it
    /// was asked for again meanwhile.
    cleaning: HashMap<String, bool>,
    /// Hosted agents with a change on its way (`exclusive(pubkey)`).
    pub(super) hosted_busy: HashSet<String>,
    /// Where agents' attention handles send their reads and writes.
    attention_calls: futures::channel::mpsc::UnboundedSender<AttentionCall>,
    /// The owner's agent list in each bound community.
    listed: listed::Listed,
    /// What each community was told about which agents run here.
    presence: presence::Announcer,
    _subscriptions: Vec<Subscription>,
    _tick: Task<()>,
    _attention: Task<()>,
}

impl Agents2 {
    fn new(
        storage: Arc<dyn Storage>,
        team_storage: Arc<dyn Storage>,
        cx: &mut Context<Self>,
    ) -> Self {
        let records = read_records(storage.as_ref());
        let teams = read_teams(team_storage.as_ref());
        let subscriptions = vec![
            cx.observe_global::<registry::AgentTypes>(|this, cx| {
                this.update(cx);
                cx.notify();
            }),
            // A plugin turned on or off brings or takes its types.
            cx.observe_global::<crate::plugins::Plugins>(|this, cx| {
                this.update(cx);
                cx.notify();
            }),
            cx.observe_global::<Hosts>(|this, cx| this.watch_hosts(cx)),
            cx.observe_global::<AppState>(|this, cx| {
                this.watch(cx);
                this.update(cx);
            }),
        ];
        // Wakes each runner, which starts any timer that is due.
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TIMER_TICK).await;
                let alive = this.update(cx, |this, cx| {
                    let pubkeys: Vec<String> = this.runners.keys().cloned().collect();
                    for pubkey in pubkeys {
                        this.drain(&pubkey, cx);
                    }
                    this.reconnect(cx);
                    this.announce(cx);
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        let (attention_calls, mut calls) = futures::channel::mpsc::unbounded::<AttentionCall>();
        let attention = cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(call) = calls.next().await {
                if this.update(cx, |this, cx| call(this, cx)).is_err() {
                    return;
                }
            }
        });
        let mut this = Self {
            storage,
            records,
            team_storage,
            teams,
            lists: Vec::new(),
            status: Status::Unavailable,
            agents: Vec::new(),
            running: Vec::new(),
            bound: HashMap::new(),
            connected: HashSet::new(),
            generation: 0,
            runners: HashMap::new(),
            serial: 0,
            cleaning: HashMap::new(),
            hosted_busy: HashSet::new(),
            attention_calls,
            listed: listed::Listed::default(),
            presence: presence::Announcer::default(),
            _subscriptions: subscriptions,
            _tick: tick,
            _attention: attention,
        };
        this.watch_hosts(cx);
        this.watch(cx);
        this
    }

    pub fn global(cx: &App) -> Option<Entity<Agents2>> {
        cx.try_global::<Agents2Global>()
            .map(|global| global.0.clone())
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// The owner's teams on this device, in any community.
    pub fn teams(&self) -> &[Team] {
        &self.teams
    }

    /// The selected community's agents, while it is ready.
    pub fn agents(&self) -> &[Agent] {
        &self.agents
    }

    /// Every agent kept running here, whichever community is selected: the
    /// viewer's agents in each joined community.
    pub fn running(&self) -> &[Agent] {
        &self.running
    }

    /// The agent of this pubkey in the selected community, if it is an
    /// Agents2 agent.
    pub fn find(&self, pubkey: &str) -> Option<&Agent> {
        self.agents.iter().find(|agent| agent.pubkey == pubkey)
    }

    /// The owner's connection to a running agent's community, which need
    /// not be the selected one: read the agent's conversations through
    /// this.
    pub fn relay(&self, pubkey: &str) -> Option<Entity<Community>> {
        let origin = http_origin(&self.agent(pubkey)?.relay)?;
        self.bound.get(&origin).map(|bound| bound.community.clone())
    }

    /// A running agent, in any community.
    fn agent(&self, pubkey: &str) -> Option<&Agent> {
        self.running.iter().find(|agent| agent.pubkey == pubkey)
    }

    /// The agent, while its community's session is ready to run it in.
    fn ready(&self, pubkey: &str) -> Option<&Agent> {
        let sessions = self.sessions();
        self.agent(pubkey)
            .filter(|agent| ready(&agent.owner, &agent.relay, &sessions))
    }

    fn sessions(&self) -> Sessions {
        self.bound
            .iter()
            .map(|(origin, bound)| {
                let viewer = bound.binding.as_ref().map(|binding| binding.viewer.clone());
                (origin.clone(), viewer)
            })
            .collect()
    }

    /// The live binding of the agent's community, if it has one now.
    fn binding_of(&self, summary: &AppAgentSummary) -> Option<&Binding> {
        self.bound
            .get(&http_origin(&summary.relay)?)?
            .binding
            .as_ref()
            .filter(|binding| binding.viewer == summary.owner)
    }

    /// The selected community, while it is bound.
    fn selected(&self, cx: &App) -> Option<&Bound> {
        self.bound.get(&selected_origin(cx)?)
    }

    /// Every saved identity on every host, in every community, deleted ones
    /// left out (`local()`).
    pub fn local(&self) -> Vec<LocalAgent> {
        let mut agents: Vec<LocalAgent> = Vec::new();
        for saved in self.saved().filter(|saved| !saved.summary.deleted) {
            if agents
                .iter()
                .any(|agent| agent.pubkey == saved.summary.pubkey)
            {
                continue;
            }
            agents.push(LocalAgent {
                pubkey: saved.summary.pubkey,
                name: saved.summary.name,
                relay: saved.summary.relay,
                owner: saved.summary.owner,
            });
        }
        agents
    }

    /// What a running agent's runner is doing, for Mission control.
    pub fn run_state(&self, pubkey: &str) -> RunState {
        let ready = self.ready(pubkey).is_some();
        match self.runners.get(pubkey) {
            Some(runner) => RunState {
                working_since: runner.working_since,
                held: runner.may_hold,
                ready,
                failed: runner.failed.clone(),
            },
            None => RunState {
                ready,
                ..RunState::default()
            },
        }
    }

    /// The host holding the agent's key.
    /// A handle that acts as agent `pubkey` while it exists here, outside
    /// any run (`AgentHandle` "works for as long as the agent exists").
    pub(crate) fn handle_of(&self, pubkey: &str, cx: &App) -> Option<AgentHandle> {
        let agent = self.live(pubkey)?;
        let host = self.host_of(pubkey)?;
        let runner = self.runners.get(pubkey)?;
        Some(AgentHandle {
            pubkey: agent.summary.pubkey.clone(),
            name: agent.summary.name.clone(),
            owner: agent.summary.owner.clone(),
            host: link(&host, cx),
            wrote: runner.wrote.clone(),
            attention: self.attention(pubkey),
        })
    }

    pub fn host_of(&self, pubkey: &str) -> Option<Entity<HostConnection>> {
        self.saved()
            .find(|saved| saved.summary.pubkey == pubkey)
            .map(|saved| saved.host)
    }

    /// Hosts that can hold a new agent's key.
    pub fn hosts(&self, cx: &App) -> Vec<Entity<HostConnection>> {
        self.lists
            .iter()
            .filter(|(_, list)| list.state == ListState::Ready)
            .map(|(_, list)| list.host.clone())
            .filter(|host| matches!(host.read(cx).state, ConnectionState::Connected { .. }))
            .collect()
    }

    /// Every saved identity, deleted ones included.
    fn saved(&self) -> impl Iterator<Item = Saved> + '_ {
        self.lists.iter().flat_map(|(_, list)| {
            list.agents.iter().map(|summary| Saved {
                summary: summary.clone(),
                host: list.host.clone(),
            })
        })
    }

    fn live(&self, pubkey: &str) -> Option<Saved> {
        self.saved()
            .find(|saved| saved.summary.pubkey == pubkey && !saved.summary.deleted)
    }

    // ----- hosts -----

    /// Follows the host list: new hosts are listed when they connect.
    fn watch_hosts(&mut self, cx: &mut Context<Self>) {
        let hosts: Vec<Entity<HostConnection>> = cx
            .try_global::<Hosts>()
            .map(|hosts| hosts.hosts.clone())
            .unwrap_or_default();
        let before = self.lists.len();
        self.lists
            .retain(|(id, _)| hosts.iter().any(|host| host.entity_id() == *id));
        let mut added = false;
        for host in hosts {
            if self.lists.iter().any(|(id, _)| *id == host.entity_id()) {
                continue;
            }
            let observe = cx.observe(&host, |this, host, cx| this.host_changed(host, cx));
            self.lists.push((
                host.entity_id(),
                HostList {
                    host: host.clone(),
                    state: ListState::Loading,
                    agents: Vec::new(),
                    turn: 0,
                    connected: false,
                    _observe: observe,
                },
            ));
            added = true;
            self.host_changed(host, cx);
        }
        if added || self.lists.len() != before {
            self.watch(cx);
            self.update(cx);
            self.recompute_status();
            cx.notify();
        }
    }

    /// Lists a host's agents whenever it connects.
    fn host_changed(&mut self, host: Entity<HostConnection>, cx: &mut Context<Self>) {
        let connected = matches!(host.read(cx).state, ConnectionState::Connected { .. });
        let Some((_, list)) = self
            .lists
            .iter_mut()
            .find(|(id, _)| *id == host.entity_id())
        else {
            return;
        };
        let was = list.connected;
        list.connected = connected;
        if connected && !was {
            self.refresh_host(host, cx).detach();
        } else if !connected && was {
            self.recompute_status();
            cx.notify();
        }
    }

    /// Reads the agent list from every host.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let tasks: Vec<Task<()>> = self
            .lists
            .iter()
            .filter(|(_, list)| list.connected)
            .map(|(_, list)| list.host.clone())
            .collect::<Vec<_>>()
            .into_iter()
            .map(|host| self.refresh_host(host, cx))
            .collect();
        cx.spawn(async move |_, _| {
            futures::future::join_all(tasks).await;
        })
    }

    fn refresh_host(&mut self, host: Entity<HostConnection>, cx: &mut Context<Self>) -> Task<()> {
        let id = host.entity_id();
        let Some((_, list)) = self.lists.iter_mut().find(|(key, _)| *key == id) else {
            return Task::ready(());
        };
        list.turn += 1;
        let turn = list.turn;
        let link = link(&host, cx);
        cx.spawn(async move |this, cx| {
            let listed = link.app_agent(AppAgentRequest::List).await;
            let _ = this.update(cx, |this, cx| {
                let Some((_, list)) = this.lists.iter_mut().find(|(key, _)| *key == id) else {
                    return;
                };
                // A newer list is on its way; this one may predate a change.
                if list.turn != turn {
                    return;
                }
                match listed {
                    Ok(AppAgentResponse::Agents(agents)) => {
                        list.agents = agents;
                        list.state = ListState::Ready;
                    }
                    Ok(_) => list.state = ListState::Error("Invalid agent list".into()),
                    Err(error) if error == super::runtime::UNSUPPORTED => {
                        list.agents.clear();
                        list.state = ListState::Unsupported;
                    }
                    Err(error) => list.state = ListState::Error(error),
                }
                this.prune();
                this.recompute_status();
                this.watch(cx);
                this.update(cx);
                cx.notify();
                this.sync(cx);
            });
        })
    }

    /// Settings whose agent is gone do nothing; they are dropped once every
    /// host has answered, so a host that is offline keeps its agents'.
    fn prune(&mut self) {
        let settled = self
            .lists
            .iter()
            .all(|(_, list)| matches!(list.state, ListState::Ready | ListState::Unsupported));
        if !settled {
            return;
        }
        let live: Vec<String> = self
            .saved()
            .filter(|saved| !saved.summary.deleted)
            .map(|saved| saved.summary.pubkey)
            .collect();
        if self.records.keys().any(|pubkey| !live.contains(pubkey)) {
            self.records.retain(|pubkey, _| live.contains(pubkey));
            if let Err(error) = write_records(self.storage.as_ref(), &self.records) {
                tracing::warn!(%error, "agent settings were not saved");
            }
        }
        // A deleted agent leaves its teams; the teams stay.
        if self
            .teams
            .iter()
            .any(|team| team.members.iter().any(|pubkey| !live.contains(pubkey)))
        {
            let teams: Vec<Team> = self
                .teams
                .iter()
                .map(|team| Team {
                    members: team
                        .members
                        .iter()
                        .filter(|pubkey| live.contains(pubkey))
                        .cloned()
                        .collect(),
                    ..team.clone()
                })
                .collect();
            if let Err(error) = self.write_teams(teams) {
                tracing::warn!(%error, "teams were not saved");
            }
        }
    }

    fn recompute_status(&mut self) {
        let states: Vec<&ListState> = self.lists.iter().map(|(_, list)| &list.state).collect();
        self.status = if states.is_empty() {
            Status::Unavailable
        } else if states.contains(&&ListState::Ready) {
            Status::Ready
        } else if states.contains(&&ListState::Loading) {
            Status::Loading
        } else {
            let error = states.iter().find_map(|state| match state {
                ListState::Error(error) => Some(error.clone()),
                _ => None,
            });
            Status::Error(error.unwrap_or_else(|| super::runtime::UNSUPPORTED.into()))
        };
    }

    // ----- the communities -----

    /// Connects the selected community, where agents are made, and each
    /// one the viewer has an agent in, deleted ones included so their
    /// cleanup can finish; lets go of the rest.
    fn watch(&mut self, cx: &mut Context<Self>) {
        let viewer = viewer(cx);
        let selected = selected_origin(cx);
        let saved: Vec<Saved> = self.saved().collect();
        let origins = wanted(
            viewer.as_deref(),
            selected.as_deref(),
            saved
                .iter()
                .map(|saved| (saved.summary.owner.as_str(), saved.summary.relay.as_str())),
        );
        // Left, or left and joined again with a new session.
        let gone: Vec<String> = self
            .bound
            .iter()
            .filter(|(origin, bound)| {
                !origins.contains(*origin) || joined(origin, cx).as_ref() != Some(&bound.community)
            })
            .map(|(origin, _)| origin.clone())
            .collect();
        for origin in gone {
            self.unbind(&origin, cx);
        }
        for origin in origins {
            if self.bound.contains_key(&origin) {
                continue;
            }
            let Some(community) = joined(&origin, cx) else {
                continue;
            };
            let observe = {
                let origin = origin.clone();
                cx.observe(&community, move |this, _, cx| this.bind(&origin, cx))
            };
            self.bound.insert(
                origin.clone(),
                Bound {
                    community,
                    binding: None,
                    retried: None,
                    _observe: observe,
                },
            );
            self.bind(&origin, cx);
        }
    }

    /// Tries each unselected community whose connection failed again,
    /// since nobody is there to press its Retry; its agents wait on it.
    fn reconnect(&mut self, cx: &mut Context<Self>) {
        let selected = selected_origin(cx);
        let now = Instant::now();
        for (origin, bound) in &mut self.bound {
            let community = bound.community.read(cx);
            let failed = matches!(community.snapshot.status, SessionStatus::Failed(_));
            if retry_due(
                selected.as_ref() == Some(origin),
                failed,
                bound.retried,
                now,
            ) {
                bound.retried = Some(now);
                community.session.retry();
            }
        }
    }

    fn unbind(&mut self, origin: &str, cx: &mut Context<Self>) {
        self.listed.forget(origin);
        if self.bound.remove(origin).is_some() {
            self.update(cx);
            cx.notify();
        }
    }

    /// Binds a community once its session is ready, and unbinds it while
    /// it is not; its agents keep running either way.
    fn bind(&mut self, origin: &str, cx: &mut Context<Self>) {
        let identity = match &cx.global::<AppState>().identity {
            IdentityState::Ready(identity) => Some(identity.clone()),
            _ => None,
        };
        let Some(bound) = self.bound.get_mut(origin) else {
            return;
        };
        let read = bound.community.read(cx);
        let session = read.session.generation();
        let next = identity
            .filter(|_| read.snapshot.status == SessionStatus::Ready)
            .map(|identity| (identity, read.snapshot.live));
        match (&mut bound.binding, &next) {
            // Retries what failed whenever the connection recovers.
            (Some(binding), Some((_, live))) => {
                let recovered = *live && !binding.live;
                let dropped = !*live && binding.live;
                binding.live = *live;
                // The agent list read no longer stands once the live
                // connection drops or the cache is cleared, and is read
                // again when it is back (`agentLibrary.clear()`,
                // `reconnect()`).
                let cleared = self.listed.read_before(origin, session);
                if cleared {
                    self.listed.set_cleared(origin, session);
                } else if dropped {
                    self.listed.set_stale(origin);
                }
                if dropped || cleared {
                    cx.notify();
                }
                if (recovered || cleared) && *live {
                    self.refresh_listings(origin, cx);
                }
                if recovered {
                    self.sync(cx);
                }
                return;
            }
            (None, None) => return,
            _ => {}
        }
        self.generation += 1;
        let generation = self.generation;
        self.listed.forget(origin);
        let binding = next.map(|(identity, live)| {
            let (session, destination) = {
                let read = bound.community.read(cx);
                (read.session.clone(), read.origin.websocket().to_owned())
            };
            Binding {
                destination,
                viewer: session.viewer().to_owned(),
                identity,
                live,
                generation,
                _events: listen(&session, origin, cx),
                session,
            }
        });
        let live = binding.is_some();
        if let Some(bound) = self.bound.get_mut(origin) {
            bound.binding = binding;
        }
        self.update(cx);
        cx.notify();
        if live {
            self.sync(cx);
        }
    }

    /// Brings the relay in line with the saved identities: publishes each
    /// name its community lacks, and finishes Delete where connected. Each step is safe to repeat; what fails is tried again on
    /// the next refresh or reconnect.
    fn sync(&mut self, cx: &mut Context<Self>) {
        // Its sync reads the community's agent records too.
        self.sync_listings(cx);
        let saved: Vec<Saved> = self.saved().collect();
        for identity in saved {
            if !identity.summary.deleted {
                let link = link(&identity.host, cx);
                let name = identity.summary.name.clone();
                let pubkey = identity.summary.pubkey.clone();
                cx.background_spawn(async move {
                    if let Err(error) = link
                        .app_agent(AppAgentRequest::PublishProfile { pubkey })
                        .await
                    {
                        tracing::warn!(agent = %name, %error, "agent profile was not published");
                    }
                })
                .detach();
            } else if self.binding_of(&identity.summary).is_some() {
                self.cleanup(identity, cx);
            }
        }
    }

    /// As harness Delete does: leave every channel, then archive the
    /// identity so it drops out of member lists and mention suggestions.
    /// The owner signs both, so neither needs the deleted key.
    fn cleanup(&mut self, identity: Saved, cx: &mut Context<Self>) {
        let pubkey = identity.summary.pubkey.clone();
        if let Some(again) = self.cleaning.get_mut(&pubkey) {
            *again = true;
            return;
        }
        let Some(binding) = self.binding_of(&identity.summary) else {
            return;
        };
        let generation = binding.generation;
        let session = binding.session.clone();
        let viewer = binding.identity.clone();
        self.cleaning.insert(pubkey.clone(), false);
        let link = link(&identity.host, cx);
        let runtime = Runtime::handle(cx);
        cx.spawn(async move |this, cx| {
            let target = pubkey.clone();
            let work = runtime.spawn(async move {
                tokio::time::timeout(
                    CLEANUP_TIMEOUT,
                    super::cleanup::clean(&session, &viewer, &target),
                )
                .await
                .unwrap_or_else(|_| Err("Deleted agent cleanup timed out".into()))
            });
            let mut outcome = work
                .await
                .unwrap_or_else(|_| Err("Deleted agent cleanup was cancelled".into()));
            if outcome.is_ok() {
                outcome = link
                    .app_agent(AppAgentRequest::Forget {
                        pubkey: pubkey.clone(),
                    })
                    .await
                    .map(drop);
            }
            let _ = this.update(cx, |this, cx| {
                let again = this.cleaning.remove(&pubkey).unwrap_or(false);
                let current = this
                    .binding_of(&identity.summary)
                    .is_some_and(|binding| binding.generation == generation);
                match outcome {
                    Ok(()) => {
                        this.unlist(&pubkey);
                        this.refresh(cx).detach();
                    }
                    Err(error) if current => {
                        tracing::warn!(%error, "deleted agent cleanup will be retried");
                        if again {
                            this.cleanup(identity, cx);
                        }
                    }
                    Err(_) => {}
                }
            });
        })
        .detach();
    }

    // ----- the running and visible agents, and their runners -----

    /// Its saved settings or, without any, its type's defaults, kept in
    /// memory until a save stores them. Without its type there are no
    /// defaults yet.
    fn record_of(&mut self, summary: &AppAgentSummary, cx: &App) -> AgentRecord {
        if !self.records.contains_key(&summary.pubkey)
            && let Some(kind) = registry::type_of(&summary.agent_type, cx)
        {
            match defaults(&summary.pubkey, &kind) {
                Ok(record) => {
                    self.records.insert(summary.pubkey.clone(), record);
                }
                Err(error) => {
                    tracing::warn!(kind = %summary.agent_type, %error, "agent type has invalid defaults");
                }
            }
        }
        self.records
            .get(&summary.pubkey)
            .cloned()
            .unwrap_or_else(|| AgentRecord::blank(&summary.pubkey))
    }

    fn view(summary: &AppAgentSummary, record: &AgentRecord) -> Agent {
        Agent {
            pubkey: summary.pubkey.clone(),
            name: summary.name.clone(),
            agent_type: summary.agent_type.clone(),
            owner: summary.owner.clone(),
            relay: summary.relay.clone(),
            picture: summary.picture.clone(),
            attention: record.attention.clone(),
            skipped: record.skipped.clone(),
            timers: record.timers.clone(),
            attention_enabled: record.attention_on(),
            config: record.config.clone(),
        }
    }

    /// Joins identities, records and the bindings into the running and
    /// visible agents, and keeps one runner per saved agent. Runners
    /// outlive edits and reconnects; one is retired only when its agent is
    /// deleted, and its run is cancelled when the agent's type is replaced.
    fn update(&mut self, cx: &mut Context<Self>) {
        let live: Vec<AppAgentSummary> = self
            .saved()
            .filter(|saved| !saved.summary.deleted)
            .map(|saved| saved.summary)
            .collect();
        let viewer = viewer(cx);
        let sessions = self.sessions();
        let mut running = Vec::new();
        for summary in &live {
            if !alive(&summary.owner, &summary.relay, viewer.as_deref(), &sessions) {
                continue;
            }
            let record = self.record_of(summary, cx);
            running.push(Self::view(summary, &record));
        }
        let selected = selected_origin(cx);
        let agents: Vec<Agent> = running
            .iter()
            .filter(|agent| shown(&agent.owner, &agent.relay, selected.as_deref(), &sessions))
            .cloned()
            .collect();
        let local = cx.default_global::<LocalActivity>().clone();
        self.runners.retain(|pubkey, _| {
            let kept = live.iter().any(|summary| &summary.pubkey == pubkey);
            if !kept {
                local.clear(None, Some(pubkey));
            }
            kept
        });
        let mut drains = Vec::new();
        for summary in &live {
            let kind = registry::type_of(&summary.agent_type, cx);
            // Turning attention off voids the watch work it queued, even if
            // it is turned on again before that work would run.
            if let Some(runner) = self.runners.get_mut(&summary.pubkey)
                && !self
                    .records
                    .get(&summary.pubkey)
                    .is_some_and(AgentRecord::attention_on)
            {
                runner.epoch += 1;
                runner
                    .queue
                    .retain(|job| !matches!(job.trigger, Trigger::Watch { .. }));
            }
            match self.runners.get_mut(&summary.pubkey) {
                None => {
                    self.serial += 1;
                    let (lifetime, lifetime_signal) = CancelSource::new();
                    self.runners.insert(
                        summary.pubkey.clone(),
                        Runner {
                            kind,
                            lifetime: Arc::new(lifetime),
                            lifetime_signal,
                            queue: VecDeque::new(),
                            running: false,
                            window: Window {
                                start: None,
                                admitted: 0,
                            },
                            seen: Bounded::default(),
                            wrote: Arc::default(),
                            compiled: None,
                            serial: self.serial,
                            may_hold: true,
                            asked: false,
                            typing: typing::Pulses::default(),
                            working_since: None,
                            failed: None,
                            activity_claimed: false,
                            epoch: 0,
                            working: Arc::default(),
                            transitioning: false,
                        },
                    );
                }
                Some(runner) => {
                    let replaced = match (&runner.kind, &kind) {
                        (Some(a), Some(b)) => !Arc::ptr_eq(&a.kind, &b.kind),
                        (None, None) => false,
                        _ => true,
                    };
                    if replaced {
                        runner.lifetime.cancel();
                        let (lifetime, signal) = CancelSource::new();
                        runner.lifetime = Arc::new(lifetime);
                        runner.lifetime_signal = signal;
                        runner.kind = kind;
                        drains.push(summary.pubkey.clone());
                    }
                }
            }
        }
        if agents != self.agents || running != self.running {
            self.agents = agents;
            self.running = running;
            cx.notify();
        }
        // Resume runners whose queue paused while their community was not
        // ready, and claim or release each agent as its community becomes
        // ready or not, so another GUI takes over without waiting for an
        // event or tick.
        let connected: HashSet<String> = self
            .running
            .iter()
            .filter(|agent| ready(&agent.owner, &agent.relay, &sessions))
            .map(|agent| agent.pubkey.clone())
            .collect();
        for (pubkey, runner) in &self.runners {
            if !runner.queue.is_empty()
                || connected.contains(pubkey) != self.connected.contains(pubkey)
            {
                drains.push(pubkey.clone());
            }
        }
        self.connected = connected;
        for pubkey in drains {
            self.drain(&pubkey, cx);
        }
        self.announce(cx);
    }

    /// Tells each community which agents this copy runs there (owner
    /// presence): see [`presence::Announcer::plan`].
    fn announce(&mut self, cx: &mut Context<Self>) {
        let mut online: HashMap<String, std::collections::BTreeSet<String>> = HashMap::new();
        for agent in &self.running {
            if let Some(origin) = http_origin(&agent.relay)
                && self.ready(&agent.pubkey).is_some()
                && self.runs(&agent.pubkey)
            {
                online
                    .entry(origin)
                    .or_default()
                    .insert(agent.pubkey.clone());
            }
        }
        let bound: Vec<(String, Option<usize>)> = self
            .bound
            .iter()
            .map(|(origin, bound)| {
                let session = bound
                    .binding
                    .as_ref()
                    .map(|binding| Arc::as_ptr(&binding.session) as usize);
                (origin.clone(), session)
            })
            .collect();
        let yielded: HashSet<String> = self
            .runners
            .keys()
            .filter(|pubkey| self.yielded(pubkey))
            .cloned()
            .collect();
        let announcements = self.presence.plan(
            &bound,
            &online,
            |pubkey| yielded.contains(pubkey),
            Instant::now(),
        );
        let runtime = Runtime::handle(cx);
        for announcement in announcements {
            let Some(session) = self
                .bound
                .get(&announcement.origin)
                .and_then(|bound| bound.binding.as_ref())
                .map(|binding| binding.session.clone())
            else {
                continue;
            };
            let (origin, id) = (announcement.origin.clone(), announcement.id);
            let sent = runtime.spawn(async move {
                for (status, agents) in [
                    ("offline", announcement.offline),
                    ("online", announcement.online),
                ] {
                    for batch in agents.chunks(presence::BATCH) {
                        if !session
                            .publish_agent_presence(status, batch.to_vec())
                            .await?
                        {
                            return Ok(false);
                        }
                    }
                }
                Ok(true)
            });
            cx.spawn(async move |this, cx| {
                let outcome = sent
                    .await
                    .unwrap_or_else(|_| Err("Agent presence stopped".to_string()));
                let _ = this.update(cx, |this, _| this.presence.finished(&origin, id, outcome));
            })
            .detach();
        }
    }

    /// Whether this copy runs the agent: its type can run it, and no other
    /// copy of the app holds it.
    fn runs(&self, pubkey: &str) -> bool {
        self.runners.get(pubkey).is_some_and(|runner| {
            runner.kind.as_ref().is_some_and(|kind| kind.kind.runs())
                && runner.asked
                && runner.may_hold
        })
    }

    /// Whether another copy of the app runs an agent this one could.
    fn yielded(&self, pubkey: &str) -> bool {
        self.runners.get(pubkey).is_some_and(|runner| {
            runner.kind.as_ref().is_some_and(|kind| kind.kind.runs()) && !runner.may_hold
        }) && self.ready(pubkey).is_some()
    }

    /// Gives a community's live events to the agents in it.
    fn dispatch(&mut self, batch: LiveBatch, origin: &str, cx: &mut Context<Self>) {
        let events: Vec<Arc<EventData>> = batch
            .events
            .iter()
            .map(|event| Arc::new(EventData::from(event.as_ref())))
            .collect();
        let mut drains = Vec::new();
        let now = Instant::now();
        let sessions = self.sessions();
        let local = cx.default_global::<LocalActivity>().clone();
        for agent in &self.running {
            if !hears(&agent.owner, &agent.relay, origin, &sessions) {
                continue;
            }
            let community = agent
                .attention_enabled
                .then(|| self.relay(&agent.pubkey))
                .flatten();
            let Some(runner) = self.runners.get_mut(&agent.pubkey) else {
                continue;
            };
            if !runner.kind.as_ref().is_some_and(|kind| kind.kind.runs()) {
                continue;
            }
            let watches = match &runner.compiled {
                Some((attention, watches)) if Arc::ptr_eq(attention, &agent.attention) => {
                    watches.clone()
                }
                _ => {
                    let watches = Arc::new(compile(&agent.attention));
                    runner.compiled = Some((agent.attention.clone(), watches.clone()));
                    watches
                }
            };
            for event in &events {
                if !runner.seen.insert(&event.id) {
                    continue;
                }
                // An agent never hears itself; its own events keep
                // reactions to them from its watches.
                if event.pubkey == agent.pubkey {
                    runner
                        .wrote
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(&event.id);
                    continue;
                }
                let wrote = runner.wrote.clone();
                let wrote =
                    move |id: &str| wrote.lock().unwrap_or_else(|p| p.into_inner()).contains(id);
                // Off, or outside the agent's channels, no watch hears it.
                let watching = community
                    .as_ref()
                    .is_some_and(|community| member(community, &agent.pubkey, event, cx));
                let watches: &[CompiledWatch] = if watching { &watches } else { &[] };
                for trigger in heard(&agent.pubkey, &agent.owner, event, &wrote, watches) {
                    let matched = match &trigger {
                        Trigger::Watch { slug, .. } => agent
                            .attention
                            .get(slug)
                            .map(|object| (object.clone(), runner.epoch)),
                        _ => None,
                    };
                    let mut job = Job {
                        trigger,
                        channel: batch.channel.clone(),
                        work: None,
                        matched,
                    };
                    let scope = work_scope(agent).filter(|_| runner.activity_claimed);
                    if let Some(scope) = &scope {
                        show_work(&local, scope, &agent.pubkey, &mut job);
                    }
                    if admit(&mut runner.window, runner.queue.len(), now) {
                        runner.queue.push_back(job);
                    } else {
                        if let Some(work) = job.work {
                            work.finish(WorkOutcome::failed(BUSY));
                        }
                        tracing::warn!(agent = %agent.name, "agent dropped an event");
                    }
                }
            }
            if !runner.queue.is_empty() {
                drains.push(agent.pubkey.clone());
            }
        }
        for pubkey in drains {
            self.drain(&pubkey, cx);
        }
    }

    /// The due timer of a running agent, recording its run.
    fn due(&mut self, agent: &Agent, epoch: u64) -> Option<Job> {
        // Off, timers keep their saved state; one overdue runs once when
        // back on.
        if !agent.attention_enabled {
            return None;
        }
        let record = self
            .records
            .get(&agent.pubkey)
            .cloned()
            .unwrap_or_else(|| AgentRecord::blank(&agent.pubkey));
        let (timers, next) = due_timer(&agent.attention, &agent.timers, now_secs());
        if timers != *record.timers {
            let record = AgentRecord {
                timers: Arc::new(timers),
                ..record
            };
            // Unsaved, it would run again at once; it waits for storage
            // instead.
            if let Err(error) = self.write(record) {
                tracing::warn!(agent = %agent.name, %error, "agent timer state was not saved");
                return None;
            }
        }
        let (slug, timer, ran) = next?;
        let interest = interest(&agent.attention, &timer.interest_id);
        let matched = agent
            .attention
            .get(&slug)
            .map(|object| (object.clone(), epoch));
        Some(Job {
            trigger: Trigger::Timer {
                spent: timer_spent(&timer, &ran, ran.next_due),
                slug,
                timer,
                interest,
            },
            channel: None,
            work: None,
            matched,
        })
    }

    /// One run at a time per agent: events in arrival order, then a due
    /// timer. While its community's session is not ready the queue pauses; a type that
    /// cannot run drops what is queued. Either way the host lets another
    /// GUI have the agent. Before each run, on every wake, the host is
    /// asked whether this GUI holds the agent; one that does not drops
    /// what is queued. A run that ignores its deadline stops holding the
    /// queue when the deadline passes.
    fn drain(&mut self, pubkey: &str, cx: &mut Context<Self>) {
        let Some(runner) = self.runners.get_mut(pubkey) else {
            return;
        };
        if runner.running {
            return;
        }
        runner.running = true;
        let serial = runner.serial;
        let pubkey = pubkey.to_owned();
        cx.spawn(async move |this, cx| {
            loop {
                let (link, seen, claim) =
                    match this.update(cx, |this, cx| this.step(&pubkey, serial, cx)) {
                        Ok(Step::Claim(link, seen)) => (link, seen, true),
                        Ok(Step::Release(link, seen)) => (link, seen, false),
                        Ok(Step::Idle) | Err(_) => break,
                    };
                let request = match claim {
                    true => AppAgentRequest::Claim {
                        pubkey: pubkey.clone(),
                    },
                    false => AppAgentRequest::Release {
                        pubkey: pubkey.clone(),
                    },
                };
                let answer = link.app_agent(request).await;
                if !claim {
                    if let Err(error) = answer
                        && error != UNSUPPORTED
                    {
                        tracing::warn!(agent = %pubkey, %error, "agent was not released");
                    }
                    // Its community may have become ready again, or it may
                    // have gained its type, meanwhile.
                    match this.update(cx, |this, _| this.seen(&pubkey) != seen) {
                        Ok(true) => continue,
                        _ => break,
                    }
                }
                let claimed = claimed(&pubkey, answer);
                let next = this.update(cx, |this, cx| {
                    this.start(&pubkey, serial, claimed, &seen, cx)
                });
                let (kind, delivery, lifetime, source) = match next {
                    Ok(Start::Run(kind, delivery, lifetime, source)) => {
                        (kind, delivery, lifetime, source)
                    }
                    Ok(Start::Changed) => continue,
                    Ok(Start::Idle) | Err(_) => break,
                };
                let timeout = kind.timeout();
                let (mut delivery, agent) = *delivery;
                let working = delivery.working.clone();
                let run = {
                    let working = working.clone();
                    let lifetime = lifetime.clone();
                    cx.spawn(async move |cx| {
                        let pubkey = delivery.agent.pubkey.clone();
                        if let Some(asked) = asked(&delivery.trigger, &pubkey, &agent.owner) {
                            let offered = match cx.update(|cx| kind.models(&agent, cx)) {
                                Some(listing) => match listing.await {
                                    Ok(models) => Some(models),
                                    Err(error) => {
                                        tracing::warn!(agent = %pubkey, %error, "agent type could not list its models");
                                        None
                                    }
                                },
                                None => None,
                            };
                            (delivery.model, delivery.effort) = picked(asked, offered.as_ref());
                            if lifetime.is_cancelled() {
                                return Ok(());
                            }
                        }
                        let result = cx.update(|cx| kind.run(delivery, cx)).await;
                        // The run's own end settles its hold; a handover
                        // deadline only moves the queue on.
                        match &result {
                            Ok(()) => working.release(),
                            Err(error) => working.release_with(WorkOutcome::failed(error.to_string())),
                        }
                        result
                    })
                };
                let deadline = cx.background_executor().timer(timeout);
                let ended = futures::future::select(
                    run,
                    futures::future::select(
                        Box::pin(deadline),
                        Box::pin(async move { lifetime.cancelled().await }),
                    ),
                )
                .await;
                source.cancel();
                let failed = match ended {
                    futures::future::Either::Left((Err(error), _)) => {
                        tracing::error!(agent = %pubkey, %error, "agent run failed");
                        Some(error.to_string())
                    }
                    futures::future::Either::Left((Ok(()), _)) => None,
                    // The run keeps going unfenced; the queue moves on.
                    futures::future::Either::Right((_, run)) => {
                        run.detach();
                        None
                    }
                };
                let _ = this.update(cx, |this, _| {
                    if let Some(runner) = this.runners.get_mut(&pubkey) {
                        runner.working_since = None;
                        runner.failed = failed.map(|error| (now_ms(), error));
                    }
                });
            }
            let _ = this.update(cx, |this, _| {
                if let Some(runner) = this.runners.get_mut(&pubkey)
                    && runner.serial == serial
                {
                    runner.running = false;
                }
            });
        })
        .detach();
    }

    /// The agent while its community is ready, and its runnable type.
    fn seen(&self, pubkey: &str) -> Seen {
        Seen {
            agent: self.ready(pubkey).cloned(),
            kind: self
                .runners
                .get(pubkey)
                .and_then(|runner| runner.kind.clone())
                .filter(|kind| kind.kind.runs()),
        }
    }

    /// Whether the runner asks the host to hold its agent or lets it go.
    /// An agent whose community is not ready, or of a type that cannot run
    /// here, is left to another GUI; what it queued while ready is dropped.
    fn step(&mut self, pubkey: &str, serial: u64, cx: &mut Context<Self>) -> Step {
        let seen = self.seen(pubkey);
        let host = self.host_of(pubkey);
        let Some(runner) = self.runners.get_mut(pubkey).filter(|r| r.serial == serial) else {
            return Step::Idle;
        };
        if runner.transitioning {
            return Step::Idle;
        }
        if seen.agent.is_none() || seen.kind.is_none() {
            if seen.agent.is_some() {
                cx.default_global::<LocalActivity>()
                    .clear(None, Some(pubkey));
                runner.queue.clear();
            }
            if !std::mem::replace(&mut runner.may_hold, false) {
                return Step::Idle;
            }
            return match host {
                Some(host) => Step::Release(link(&host, cx), seen),
                None => Step::Idle,
            };
        }
        match host {
            Some(host) => Step::Claim(link(&host, cx), seen),
            None => Step::Idle,
        }
    }

    /// Starts the runner's next job, if this GUI holds the agent and it has
    /// one to run now.
    fn start(
        &mut self,
        pubkey: &str,
        serial: u64,
        claimed: bool,
        seen: &Seen,
        cx: &mut Context<Self>,
    ) -> Start {
        let current = self.seen(pubkey);
        let Some(runner) = self.runners.get_mut(pubkey).filter(|r| r.serial == serial) else {
            return Start::Idle;
        };
        runner.may_hold = claimed;
        runner.asked = true;
        let local = cx.default_global::<LocalActivity>().clone();
        // Another GUI or copy of the app holds it and runs these.
        if !claimed {
            local.clear(None, Some(pubkey));
            runner.activity_claimed = false;
            runner.queue.clear();
            return Start::Idle;
        }
        // A change of type may have taken the runner while the claim was
        // pending.
        if runner.transitioning {
            return Start::Idle;
        }
        if current != *seen {
            return Start::Changed;
        }
        let (Some(agent), Some(kind)) = (current.agent, current.kind) else {
            return Start::Idle;
        };
        runner.activity_claimed = true;
        if let Some(scope) = work_scope(&agent) {
            for queued in runner.queue.iter_mut() {
                show_work(&local, &scope, pubkey, queued);
            }
        }
        let lifetime = runner.lifetime_signal.clone();
        let epoch = runner.epoch;
        let mut queued = None;
        while let Some(job) = runner.queue.pop_front() {
            queued = rematched(epoch, &agent, job);
            if queued.is_some() {
                break;
            }
        }
        let job = match queued {
            Some(job) => Some(job),
            None => self.due(&agent, epoch),
        };
        let Some(job) = job else {
            return Start::Idle;
        };
        let Some(host) = self.host_of(pubkey) else {
            return Start::Idle;
        };
        let Some(runner) = self.runners.get(pubkey) else {
            return Start::Idle;
        };
        let (source, cancel) = CancelSource::new();
        let handle = AgentHandle {
            pubkey: agent.pubkey.clone(),
            name: agent.name.clone(),
            owner: agent.owner.clone(),
            host: link(&host, cx),
            wrote: runner.wrote.clone(),
            attention: self.attention(&agent.pubkey),
        };
        let counted = runner.working.clone();
        if let Some(runner) = self.runners.get_mut(pubkey) {
            runner.working_since = Some(now_ms());
        }
        let working = self.working(&handle, &job, &lifetime, cx).counted(counted);
        let current = {
            let (calls, pubkey, job, lifetime) = (
                self.attention_calls.clone(),
                agent.pubkey.clone(),
                job.clone(),
                lifetime.clone(),
            );
            Arc::new(move || -> futures::future::BoxFuture<'static, bool> {
                let (reply, answer) = futures::channel::oneshot::channel();
                let (pubkey, job, lifetime) = (pubkey.clone(), job.clone(), lifetime.clone());
                let asked = calls.unbounded_send(Box::new(
                    move |this: &mut Agents2, _: &mut Context<Agents2>| {
                        let current =
                            !lifetime.is_cancelled() && this.still_current(&pubkey, serial, &job);
                        let _ = reply.send(current);
                    },
                ));
                Box::pin(async move { asked.is_ok() && answer.await.unwrap_or(false) })
            }) as super::runtime::Current
        };
        let team = teams::team_prompt(&self.teams, |pubkey| self.live_name(pubkey), &agent.pubkey);
        let delivery = Delivery {
            trigger: job.trigger,
            channel: job.channel,
            agent: handle,
            config: agent.config.clone(),
            model: None,
            effort: None,
            team: (!team.is_empty()).then_some(team),
            working,
            cancel,
            current,
        };
        Start::Run(kind.kind, Box::new((delivery, agent)), lifetime, source)
    }

    /// [`still_current`] for the runner `serial` of agent `pubkey`.
    fn still_current(&self, pubkey: &str, serial: u64, job: &Job) -> bool {
        let Some(runner) = self.runners.get(pubkey).filter(|r| r.serial == serial) else {
            return false;
        };
        self.agent(pubkey)
            .is_some_and(|agent| still_current(runner.epoch, agent, job))
    }

    /// Counts a run as working until it and everything it handed to
    /// [`Working::until`] have settled, however long that takes. A mention
    /// run shows as local work meanwhile, and as typing in its thread to
    /// everyone from its first `until`: a runtime that ignores the mention
    /// never shows typing. Retiring the agent takes its work away.
    fn working(
        &self,
        agent: &AgentHandle,
        job: &Job,
        lifetime: &Cancellation,
        cx: &mut Context<Self>,
    ) -> Working {
        let runtime = Runtime::handle(cx);
        let Trigger::Mention { event } = &job.trigger else {
            return Working::none(runtime);
        };
        let (stopped, ended) = futures::channel::oneshot::channel::<()>();
        let ends: Vec<Box<dyn FnOnce() + Send>> = vec![Box::new(move || {
            let _ = stopped.send(());
        })];
        let channel = job
            .channel
            .clone()
            .or_else(|| event.tag("h").map(str::to_owned));
        let mut working = Working::new(ends, job.work.clone(), runtime.clone());
        if let Some(channel) = channel {
            // Typing to everyone in the channel, from its first hand-off,
            // where its turn answers, read the same way: in a Me chat, the
            // channel.
            let pulses = self
                .runners
                .get(&agent.pubkey)
                .map(|runner| runner.typing.clone())
                .unwrap_or_default();
            let top_level = match self
                .relay(&agent.pubkey)
                .and_then(|community| crate::agent_plugins::owner_relay::of(&community, cx))
            {
                Some((_, relay)) => relay.me_chat(&agent.owner, &channel),
                None => async { Ok(false) }.boxed(),
            };
            let (signer, event, pulse_runtime) = (agent.clone(), event.clone(), runtime);
            working = working.on_first_hold(move || {
                // Straight to the host, so pulses stay out of what it wrote.
                let publish: typing::Publish = Arc::new(move |pulse| {
                    let signer = signer.clone();
                    async move { signer.signal(pulse).await }.boxed()
                });
                typing::pulse_typing(publish, top_level, &event, &channel, pulses, &pulse_runtime)
            });
        }
        let (aborted, lifetime) = (working.clone(), lifetime.clone());
        cx.background_spawn(async move {
            let cancelled = std::pin::pin!(lifetime.cancelled());
            if let futures::future::Either::Left(_) =
                futures::future::select(cancelled, ended).await
            {
                aborted.abort();
            }
        })
        .detach();
        working
    }

    /// The owner's channel templates on this device, each with the agents
    /// it adds that are the owner's agents in `pubkey`'s community, as the
    /// create dialog offers a template's.
    pub(crate) fn channel_templates(
        &self,
        pubkey: &str,
        cx: &App,
    ) -> Vec<crate::buzz_mcp::ChannelTemplate> {
        let Some(templates) = crate::channel_templates::ChannelTemplates::global(cx) else {
            return Vec::new();
        };
        let relay = self
            .running
            .iter()
            .chain(&self.agents)
            .find(|agent| agent.pubkey == pubkey)
            .map(|agent| agent.relay.clone());
        let here: HashSet<&str> = self
            .running
            .iter()
            .chain(&self.agents)
            .filter(|agent| Some(&agent.relay) == relay.as_ref())
            .map(|agent| agent.pubkey.as_str())
            .collect();
        templates
            .read(cx)
            .templates()
            .iter()
            .map(|template| crate::buzz_mcp::ChannelTemplate {
                name: template.name.clone(),
                canvas: template.canvas.clone(),
                agents: crate::channel_templates::template_agents(template, &self.teams, |key| {
                    here.contains(key)
                }),
            })
            .collect()
    }

    // ----- changes -----

    /// Creates a team, or replaces the one with `change.id`. Members must
    /// be agents on this device.
    pub fn save_team(
        &mut self,
        change: TeamChange,
        cx: &mut Context<Self>,
    ) -> Result<Team, String> {
        if self.lists.is_empty() {
            return Err(super::runtime::UNSUPPORTED.into());
        }
        let team = teams::validate(change, &self.teams, |pubkey| self.live(pubkey).is_some())?;
        self.write_teams(teams::with(&self.teams, team.clone()))?;
        cx.notify();
        Ok(team)
    }

    /// Deletes the team; its agents are untouched.
    pub fn remove_team(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let teams: Vec<Team> = self
            .teams
            .iter()
            .filter(|team| team.id != id)
            .cloned()
            .collect();
        self.write_teams(teams)?;
        cx.notify();
        Ok(())
    }

    /// Saves the teams; memory changes only once storage has taken them.
    fn write_teams(&mut self, teams: Vec<Team>) -> Result<(), String> {
        write_teams(self.team_storage.as_ref(), &teams)?;
        self.teams = teams;
        Ok(())
    }

    /// An existing agent's name, on any host.
    fn live_name(&self, pubkey: &str) -> Option<&str> {
        self.lists.iter().find_map(|(_, list)| {
            list.agents
                .iter()
                .find(|summary| summary.pubkey == pubkey && !summary.deleted)
                .map(|summary| summary.name.as_str())
        })
    }

    /// Saves one record; memory changes only once storage has taken it.
    fn write(&mut self, record: AgentRecord) -> Result<(), String> {
        let mut records = self.records.clone();
        records.insert(record.pubkey.clone(), record);
        write_records(self.storage.as_ref(), &records)?;
        self.records = records;
        Ok(())
    }

    /// Makes an agent of `type_key` named `name` for the selected
    /// community, with its key on `host` (the embedded host by default),
    /// and the picture and config it was made with. A picture that fails
    /// to save does not fail the create.
    pub fn create(
        &mut self,
        type_key: &str,
        name: &str,
        made: Made,
        host: Option<Entity<HostConnection>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Agent, String>> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Task::ready(Err("Name the agent".into()));
        }
        self.add(
            type_key,
            host,
            made,
            cx,
            |request_id, destination, owner, agent_type| AppAgentRequest::CreatePrepare {
                request_id,
                destination,
                owner,
                agent_type,
                name,
            },
        )
    }

    /// Makes a new agent from one shared in the community: a new identity
    /// that carries what was shared, as a brought-over one does.
    pub fn port_shared(
        &mut self,
        type_key: &str,
        name: &str,
        picture: Option<String>,
        instructions: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<Agent, String>> {
        let carry = (!instructions.trim().is_empty()).then(|| buzz_host::api::Carry {
            instructions: Some(instructions.to_owned()),
            workspace: None,
        });
        let made = Made {
            picture,
            carry,
            ..Made::default()
        };
        self.create(type_key, name, made, None, cx)
    }

    /// Identities earlier agent systems saved on the connected hosts that
    /// are not Agents yet. A host from before this lists none.
    pub fn portable(&self, cx: &mut Context<Self>) -> Task<Result<Portables, String>> {
        let hosts = self.hosts(cx);
        if hosts.is_empty() {
            return Task::ready(Err("Agents run only in the desktop app".into()));
        }
        let own: HashSet<String> = self.local().into_iter().map(|agent| agent.pubkey).collect();
        let asks: Vec<_> = hosts
            .into_iter()
            .map(|host| {
                let link = link(&host, cx);
                (host, link)
            })
            .collect();
        cx.spawn(async move |_, _| {
            let mut found = Portables::default();
            for (host, link) in asks {
                match link.app_agent(AppAgentRequest::Portable).await {
                    Ok(AppAgentResponse::Portable(listed)) => {
                        found.warnings.extend(listed.warnings);
                        found.agents.extend(
                            listed
                                .agents
                                .into_iter()
                                .filter(|agent| !own.contains(&agent.pubkey))
                                .map(|agent| Portable {
                                    agent,
                                    host: host.clone(),
                                }),
                        );
                    }
                    Err(error) if error == UNSUPPORTED => {}
                    Err(error) => found.warnings.push(error),
                    Ok(_) => found
                        .warnings
                        .push("The host answered with something else".into()),
                }
            }
            Ok(found)
        })
    }

    /// Takes one over as an agent of `type_key` in the selected community,
    /// keeping its key, name and picture, and the instructions and
    /// workspace the type adopts. Its other settings stay behind.
    pub fn port(
        &mut self,
        portable: &Portable,
        type_key: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<Agent, String>> {
        // A hosted agent's key stays with its provider (`port`).
        if super::is_hosted(type_key, cx) {
            return Task::ready(Err("A hosted agent's key stays with its provider".into()));
        }
        let (source, pubkey) = (portable.agent.source, portable.agent.pubkey.clone());
        self.add(
            type_key,
            Some(portable.host.clone()),
            Made::default(),
            cx,
            move |request_id, destination, owner, agent_type| AppAgentRequest::PortPrepare {
                request_id,
                source,
                pubkey,
                destination,
                owner,
                agent_type,
            },
        )
    }

    /// Saves a new identity of `type_key` in the selected community with the
    /// type's default settings, adopting what a brought-over one carried:
    /// `prepare` asks its host for the key, which the owner attests.
    fn add(
        &mut self,
        type_key: &str,
        host: Option<Entity<HostConnection>>,
        made: Made,
        cx: &mut Context<Self>,
        prepare: impl FnOnce(String, String, String, String) -> AppAgentRequest + 'static,
    ) -> Task<Result<Agent, String>> {
        let Some(binding) = self.selected(cx).and_then(|bound| bound.binding.as_ref()) else {
            return Task::ready(Err("Connect to a community first".into()));
        };
        // A hosted agent's key stays with its provider: nothing local
        // becomes one (`add`).
        if super::is_hosted(type_key, cx) {
            return Task::ready(Err("A hosted agent's key stays with its provider".into()));
        }
        let Some(kind) = registry::type_of(type_key, cx) else {
            return Task::ready(Err("That agent type is not available".into()));
        };
        let host = host.or_else(|| {
            cx.try_global::<Hosts>()?
                .hosts
                .iter()
                .find(|host| host.read(cx).remote.is_none())
                .cloned()
        });
        let Some(host) = host else {
            return Task::ready(Err("Agents run only in the desktop app".into()));
        };
        let mut settings = match defaults("", &kind) {
            Ok(settings) => settings,
            Err(error) => return Task::ready(Err(error)),
        };
        if let Some(config) = made.config {
            settings.config = config;
        }
        let link = link(&host, cx);
        let identity = binding.identity.clone();
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = prepare(
            request_id.clone(),
            binding.destination.clone(),
            binding.viewer.clone(),
            type_key.to_owned(),
        );
        cx.spawn(async move |this, cx| {
            // A brought-over agent's type adopts what it kept.
            let (pubkey, settings) = match link.app_agent(request).await? {
                AppAgentResponse::Prepared { pubkey } => match &made.carry {
                    Some(carry) if kind.kind.adopts() => {
                        let config = kind.kind.adopt(carry, settings.config.clone());
                        (pubkey, AgentRecord { config, ..settings })
                    }
                    _ => (pubkey, settings),
                },
                AppAgentResponse::PortPrepared { pubkey, carry } if kind.kind.adopts() => {
                    let config = kind.kind.adopt(&carry, settings.config.clone());
                    (pubkey, AgentRecord { config, ..settings })
                }
                AppAgentResponse::PortPrepared { pubkey, .. } => (pubkey, settings),
                _ => return Err("The host answered with something else".into()),
            };
            let auth = identity
                .attest_agent(&pubkey)
                .map_err(|error| error.to_string())?;
            let AppAgentResponse::Created(summary) = link
                .app_agent(AppAgentRequest::CreateCommit { request_id, auth })
                .await?
            else {
                return Err("The host answered with something else".into());
            };
            // Before the refresh, which publishes its profile with the
            // picture. The agent exists either way; its picture can be set
            // again later.
            if let Some(picture) = made.picture.filter(|picture| !picture.is_empty()) {
                let pubkey = summary.pubkey.clone();
                if let Err(error) = link
                    .app_agent(AppAgentRequest::SetPicture { pubkey, picture })
                    .await
                {
                    tracing::warn!(agent = %summary.name, %error, "agent picture was not saved");
                }
            }
            let refresh = this.update(cx, |this, cx| {
                let record = AgentRecord {
                    pubkey: summary.pubkey.clone(),
                    ..settings
                };
                // The agent exists either way; unsaved settings start from
                // the defaults.
                if let Err(error) = this.write(record) {
                    tracing::warn!(agent = %summary.name, %error, "agent settings were not saved");
                }
                this.refresh(cx)
            });
            if let Ok(refresh) = refresh {
                refresh.await;
            }
            this.update(cx, |this, cx| {
                let record = this.record_of(&summary, cx);
                Self::view(&summary, &record)
            })
            .map_err(|_| "Agents stopped".to_string())
        })
    }

    /// Changes an agent's name, picture, config or attention.
    pub fn save(
        &mut self,
        pubkey: &str,
        change: AgentChange,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let Some(identity) = self.live(pubkey) else {
            return Task::ready(Err("No such agent on this device".into()));
        };
        if change.config.is_some()
            && self
                .runners
                .get(pubkey)
                .is_some_and(|runner| runner.transitioning)
        {
            return Task::ready(Err(format!(
                "{}'s provider is already changing",
                identity.summary.name
            )));
        }
        let name = change.name.as_deref().map(str::trim).map(str::to_owned);
        if name.as_deref() == Some("") {
            return Task::ready(Err("Name the agent".into()));
        }
        let mut record = self.record_of(&identity.summary, cx);
        if let Some(config) = change.config.clone() {
            record.config = config;
        }
        if let Some(enabled) = change.attention_enabled {
            record.attention_enabled = enabled.then_some(true);
        }
        let now = now_secs();
        for (slug, value) in &change.attention {
            let schedule = change.restart.contains(slug).then_some(Schedule::Restart);
            record = match set_attention(&record, slug, value.as_ref(), now, schedule) {
                Ok(record) => record,
                Err(error) => return Task::ready(Err(error)),
            };
        }
        // As the spec's writer: an Interest stays while anything uses it.
        for (slug, value) in &change.attention {
            let Some((attention::Space::Interest, id)) = attention::parse_slug(slug) else {
                continue;
            };
            if value.is_none()
                && record
                    .attention
                    .values()
                    .any(|object| object.value.interest_id() == Some(id))
            {
                return Task::ready(Err(
                    "Remove or move the watches and timers that use this Interest first".into(),
                ));
            }
        }
        if (change.config.is_some()
            || !change.attention.is_empty()
            || change.attention_enabled.is_some())
            && let Err(error) = self.write(record)
        {
            return Task::ready(Err(error));
        }
        self.update(cx);
        cx.notify();
        let name = name.filter(|name| *name != identity.summary.name);
        let picture = change
            .picture
            .map(|picture| picture.trim().to_owned())
            .filter(|picture| identity.summary.picture.as_ref() != Some(picture));
        if name.is_none() && picture.is_none() {
            return Task::ready(Ok(()));
        }
        let link = link(&identity.host, cx);
        let pubkey = pubkey.to_owned();
        cx.spawn(async move |this, cx| {
            if let Some(picture) = picture {
                let pubkey = pubkey.clone();
                link.app_agent(AppAgentRequest::SetPicture { pubkey, picture })
                    .await
                    .map_err(|error| {
                        if error == UNSUPPORTED {
                            PICTURES_UNSUPPORTED.to_string()
                        } else {
                            error
                        }
                    })?;
            }
            if let Some(name) = name {
                link.app_agent(AppAgentRequest::Rename { pubkey, name })
                    .await?;
            }
            // Shows the change and publishes it as the agent's profile.
            if let Ok(refresh) = this.update(cx, |this, cx| this.refresh(cx)) {
                refresh.await;
            }
            Ok(())
        })
    }

    /// Previews what moving the agent to `type_key` does, from the same
    /// migration [`Self::retype`] uses, without changing it. Works for an
    /// agent whose type is no longer available, which carries nothing.
    pub fn preview_retype(
        &mut self,
        pubkey: &str,
        type_key: &str,
        cx: &App,
    ) -> Result<RetypePreview, String> {
        let identity = self.live(pubkey).ok_or("No such agent on this device")?;
        let kind = registry::type_of(type_key, cx).ok_or("That agent type is not available")?;
        if super::is_hosted(type_key, cx) {
            return Err(HOSTED_RETYPE.into());
        }
        let record = self.record_of(&identity.summary, cx);
        if type_key == identity.summary.agent_type {
            return Ok(RetypePreview {
                config: record.config,
                moved: Vec::new(),
                dropped: Vec::new(),
                settings: Vec::new(),
            });
        }
        let source = registry::type_of(&identity.summary.agent_type, cx);
        Ok(registry::retype_plan(
            source.as_ref().map(|source| source.kind.as_ref()),
            &record.config,
            kind.kind.as_ref(),
        ))
    }

    /// Makes it an agent of `type_key`, keeping its key, name, picture,
    /// communities and attention. Its new config is `config`, the reviewed
    /// destination draft, or else the type's defaults with what the old
    /// type carries and the new one adopts. Answers what of the old config
    /// stayed behind. Refused while it is answering, work it handed off
    /// included, and unless this app runs it.
    pub fn retype(
        &mut self,
        pubkey: &str,
        type_key: &str,
        config: Option<Value>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<CarryKey>, String>> {
        let Some(identity) = self.live(pubkey) else {
            return Task::ready(Err("No such agent on this device".into()));
        };
        if registry::type_of(type_key, cx).is_none() {
            return Task::ready(Err("That agent type is not available".into()));
        }
        if super::is_hosted(type_key, cx) {
            return Task::ready(Err(HOSTED_RETYPE.into()));
        }
        let old_type = identity.summary.agent_type.clone();
        if type_key == old_type {
            return Task::ready(Ok(Vec::new()));
        }
        let name = identity.summary.name.clone();
        let runner = self.runners.get_mut(pubkey);
        let working = runner
            .as_ref()
            .map_or(0, |runner| runner.working.load(Ordering::SeqCst));
        let transitioning = runner.as_ref().is_some_and(|runner| runner.transitioning);
        if let Some(refused) = retype_refusal(&name, working, runner.is_some(), transitioning) {
            return Task::ready(Err(refused));
        }
        if let Some(runner) = runner {
            runner.transitioning = true;
        }
        let link = link(&identity.host, cx);
        let (pubkey, type_key) = (pubkey.to_owned(), type_key.to_owned());
        cx.spawn(async move |this, cx| {
            let changed = async {
                // Every copy of the app on this host hears the agent; only
                // the one holding it may change what runs it.
                let held = match link
                    .app_agent(AppAgentRequest::Claim {
                        pubkey: pubkey.clone(),
                    })
                    .await
                {
                    Ok(AppAgentResponse::Claimed(held)) => held,
                    Ok(_) => true,
                    Err(error) if error == UNSUPPORTED => true,
                    Err(error) => return Err(error),
                };
                let plan = this
                    .update(cx, |this, cx| {
                        if let Some(runner) = this.runners.get_mut(&pubkey) {
                            runner.may_hold = held;
                        }
                        if !held {
                            return Err("Change its type on the device that runs it".to_string());
                        }
                        let working = this
                            .runners
                            .get(&pubkey)
                            .map_or(0, |runner| runner.working.load(Ordering::SeqCst));
                        if working > 0 {
                            return Err(answering(&name));
                        }
                        this.preview_retype(&pubkey, &type_key, cx)
                    })
                    .map_err(|_| "Agents stopped".to_string())??;
                link.app_agent(AppAgentRequest::SetType {
                    pubkey: pubkey.clone(),
                    agent_type: type_key.clone(),
                })
                .await
                .map_err(|error| {
                    if error == UNSUPPORTED {
                        TYPES_UNSUPPORTED.to_string()
                    } else {
                        error
                    }
                })?;
                let config = config.unwrap_or_else(|| plan.config.clone());
                let saved = this
                    .update(cx, |this, cx| {
                        let identity = this.live(&pubkey).ok_or("No such agent on this device")?;
                        let record = AgentRecord {
                            config,
                            ..this.record_of(&identity.summary, cx)
                        };
                        this.write(record)?;
                        // Shown, and run by the new type, before the host
                        // lists it again.
                        for (_, list) in &mut this.lists {
                            for summary in &mut list.agents {
                                if summary.pubkey == pubkey {
                                    summary.agent_type = type_key.clone();
                                }
                            }
                        }
                        this.update(cx);
                        cx.notify();
                        Ok::<(), String>(())
                    })
                    .map_err(|_| "Agents stopped".to_string())?;
                if let Err(error) = saved {
                    let restore = link
                        .app_agent(AppAgentRequest::SetType {
                            pubkey: pubkey.clone(),
                            agent_type: old_type,
                        })
                        .await;
                    if let Err(rollback) = restore {
                        return Err(format!(
                            "Could not save or restore {name}'s provider: {error}; {rollback}"
                        ));
                    }
                    return Err(error);
                }
                Ok(plan.dropped)
            }
            .await;
            let refresh = this.update(cx, |this, cx| {
                if let Some(runner) = this.runners.get_mut(&pubkey) {
                    runner.transitioning = false;
                }
                this.drain(&pubkey, cx);
                changed.is_ok().then(|| this.refresh(cx))
            });
            if let Ok(Some(refresh)) = refresh {
                refresh.await;
            }
            changed
        })
    }

    /// Deletes its key at once. Leaving its channels and archiving it
    /// follow in the background whenever its community is connected.
    pub fn remove(&mut self, pubkey: &str, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let Some(identity) = self.live(pubkey) else {
            return Task::ready(Err("No such agent on this device".into()));
        };
        let link = link(&identity.host, cx);
        let pubkey = pubkey.to_owned();
        cx.spawn(async move |this, cx| {
            link.app_agent(AppAgentRequest::Delete { pubkey }).await?;
            // Drops its settings, and starts its cleanup if its community
            // is connected.
            if let Ok(refresh) = this.update(cx, |this, cx| this.refresh(cx)) {
                refresh.await;
            }
            Ok(())
        })
    }

    /// For [`super::AgentView::save`].
    pub(crate) fn save_config(
        pubkey: &str,
        config: Value,
        cx: &mut App,
    ) -> Task<Result<(), String>> {
        let Some(service) = Self::global(cx) else {
            return Task::ready(Err("Agents is not running".into()));
        };
        service.update(cx, |this, cx| {
            this.save(
                pubkey,
                AgentChange {
                    config: Some(config),
                    ..Default::default()
                },
                cx,
            )
        })
    }

    /// Channels a watch can be scoped to, by name, from the viewer's joined
    /// roster.
    pub fn channel_choices(&self, cx: &App) -> Vec<attention::ChannelChoice> {
        let Some(bound) = self.selected(cx).filter(|bound| bound.binding.is_some()) else {
            return Vec::new();
        };
        let mut choices: Vec<_> = bound
            .community
            .read(cx)
            .snapshot
            .channels
            .iter()
            .filter(|channel| !channel.read_only)
            .map(|channel| attention::ChannelChoice {
                id: channel.id.clone(),
                name: channel.name.clone(),
            })
            .collect();
        choices.sort_by_key(|choice| choice.name.to_lowercase());
        choices
    }
}

/// Whether the agent is in the event's channel, by its relay-signed
/// roster. The stream is the owner's, so a watch, even of "all", sees only
/// what the agent itself could read; an event with no channel, or whose
/// roster is unknown, wakes no watch.
fn member(community: &Entity<Community>, agent: &str, event: &EventData, cx: &App) -> bool {
    let Some(channel) = event.tag("h") else {
        return false;
    };
    community
        .read(cx)
        .snapshot
        .channels
        .iter()
        .find(|item| item.id == channel)
        .is_some_and(|item| item.members.iter().any(|member| member == agent))
}

fn link(host: &Entity<HostConnection>, cx: &App) -> HostLink {
    HostLink {
        client: host.read(cx).client(),
        runtime: Runtime::handle(cx),
    }
}

/// The signed-in viewer.
fn viewer(cx: &App) -> Option<String> {
    match &cx.global::<AppState>().identity {
        IdentityState::Ready(identity) => Some(identity.pubkey_hex()),
        _ => None,
    }
}

/// The selected community's `https://host[:port]` origin.
fn selected_origin(cx: &App) -> Option<String> {
    AppState::current(cx).map(|community| community.read(cx).origin.http())
}

/// The joined community at `origin`, if any.
fn joined(origin: &str, cx: &App) -> Option<Entity<Community>> {
    cx.global::<AppState>()
        .communities
        .iter()
        .find(|community| community.read(cx).origin.http() == origin)
        .cloned()
}

/// Delivers the session's live events to the runners of its community's
/// agents.
fn listen(session: &Arc<Session>, origin: &str, cx: &mut Context<Agents2>) -> Task<()> {
    let mut events = session.live_events();
    let origin = origin.to_owned();
    cx.spawn(async move |this, cx| {
        while let Some(batch) = events.recv().await {
            if this
                .update(cx, |this, cx| this.dispatch(batch, &origin, cx))
                .is_err()
            {
                return;
            }
        }
    })
}

mod attention_calls;
mod hosted;
pub use hosted::is_hosted;
mod listed;
#[cfg(test)]
mod tests;
