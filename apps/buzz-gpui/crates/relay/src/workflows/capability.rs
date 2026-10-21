//! The session's workflow capability (`features/workflows/capability.ts`):
//! configuration snapshots, commands through the durable outbox, run
//! history over the fixed authenticated route, and the bounded results of
//! commands this session sent. Never an engine: the relay runs workflows.

use super::protocol::{self, CommandView};
use super::types::*;
use super::view::{self, Config, Handle, Owner, ViewSnapshot, WorkflowView};
use crate::event::{Verified, newest_first};
use crate::http::RelayHttp;
use crate::messages::Draft;
use crate::outbox::{Delivery, Operation, Outbox, SendOptions};
use crate::reader::{ReadOptions, Reader};
use futures::FutureExt;
use futures::future::BoxFuture;
use nostr::prelude::EventId;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::watch;

const CACHE_FRESH: Duration = Duration::from_secs(10);
const CACHE_RETAIN: Duration = Duration::from_secs(60);
const CACHE_ENTRIES: usize = 32;
const MAX_VIEWS: usize = 16;
const MAX_OPERATIONS: usize = 256;
const UNKNOWN_RESULT: &str = "Delivery may have succeeded, but its result is unavailable. Do not submit a new command to retry.";

pub type Definitions = WorkflowView<WorkflowDefinitions>;
pub type Runs = WorkflowView<RunPage>;
pub type DefinitionsSnapshot = ViewSnapshot<WorkflowDefinitions>;

#[derive(Clone, Debug, Default)]
struct Result_ {
    outcome: Option<Outcome>,
    run_id: Option<String>,
    error: Option<String>,
    deletion_accepted: bool,
}

struct CacheEntry {
    data: WorkflowDefinitions,
    channels: Vec<String>,
    read_at: Instant,
    used_at: Instant,
    /// Identifies this retention, for the freshness check.
    serial: u64,
}

#[derive(Default)]
struct State {
    closed: bool,
    outbox: Option<Outbox>,
    access: HashSet<String>,
    results: HashMap<String, Result_>,
    /// One-time webhook secrets by save event ID. Never on a result, an
    /// operation, the journal or an error.
    secrets: HashMap<String, String>,
    interest: HashSet<String>,
    operations: Arc<[WorkflowOperation]>,
    cache: HashMap<String, CacheEntry>,
    cache_version: u64,
    cache_serial: u64,
    views: HashMap<u64, Weak<dyn Handle>>,
    next_view: u64,
}

pub(crate) struct Inner {
    runtime: tokio::runtime::Handle,
    viewer: String,
    reader: Reader,
    http: RelayHttp,
    relay_http: String,
    state: Mutex<State>,
    operations: watch::Sender<Arc<[WorkflowOperation]>>,
    me: Weak<Inner>,
}

/// The capability handle; clones share it.
#[derive(Clone)]
pub struct Workflows {
    inner: Arc<Inner>,
}

impl Owner for Inner {
    fn closed(&self) -> bool {
        self.lock().closed
    }

    fn can_access(&self, channel: &str) -> bool {
        let state = self.lock();
        !state.closed && state.access.contains(channel)
    }

    fn release(&self, view: u64) {
        self.lock().views.remove(&view);
    }
}

fn command_view(op: &Operation) -> (u16, String, Vec<&[String]>) {
    (
        op.kind(),
        op.event.pubkey.to_hex(),
        op.event.tags.iter().map(|tag| tag.as_slice()).collect(),
    )
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("workflow lock")
    }

    /// Rebuilds the operation list from the outbox and this session's
    /// results (`rebuild`).
    fn rebuild(&self) {
        let operations: Arc<[WorkflowOperation]> = {
            let mut state = self.lock();
            let local = match (&state.outbox, state.closed) {
                (Some(outbox), false) => outbox.visible(),
                _ => Arc::new([]),
            };
            let mut operations: Vec<WorkflowOperation> = local
                .iter()
                .filter_map(|op| self.operation(&state, op))
                .collect();
            if operations.len() > MAX_OPERATIONS {
                operations.drain(..operations.len() - MAX_OPERATIONS);
            }
            let active: HashSet<&str> = operations.iter().map(|op| op.event_id.as_str()).collect();
            state.results.retain(|id, _| active.contains(id.as_str()));
            state.secrets.retain(|id, _| active.contains(id.as_str()));
            let operations: Arc<[WorkflowOperation]> = operations.into();
            state.operations = operations.clone();
            operations
        };
        // Most outbox changes are chat sends: watchers wake only on a change.
        self.operations.send_if_modified(|current| {
            let changed = current[..] != operations[..];
            if changed {
                *current = operations;
            }
            changed
        });
    }

    fn operation(&self, state: &State, op: &Operation) -> Option<WorkflowOperation> {
        // Skip other kinds before formatting anything.
        if !matches!(
            op.kind(),
            protocol::DEFINITION | protocol::TRIGGER | protocol::DELETION
        ) {
            return None;
        }
        let (kind, pubkey, tags) = command_view(op);
        if !protocol::is_operation(kind, &tags) {
            return None;
        }
        let view = CommandView {
            kind,
            pubkey: &pubkey,
            content: &op.event.content,
            tags,
        };
        let workflow = protocol::reference(&view).ok()?;
        if workflow.owner != self.viewer || !state.access.contains(&workflow.channel_id) {
            return None;
        }
        let event_id = op.id().to_hex();
        let result = state.results.get(&event_id);
        let outcome = result.and_then(|r| r.outcome).unwrap_or(match op.delivery {
            Delivery::Sending => Outcome::Pending,
            Delivery::Failed => Outcome::Rejected,
            _ => Outcome::Unknown,
        });
        let error = if result.and_then(|r| r.outcome) == Some(Outcome::Succeeded) {
            None
        } else {
            result
                .and_then(|r| r.error.clone())
                .or_else(|| op.error.clone())
        };
        Some(WorkflowOperation {
            secret_held: state.secrets.contains_key(&event_id),
            event_id,
            workflow,
            action: match kind {
                protocol::DEFINITION => Action::Save,
                protocol::DELETION => Action::Delete,
                _ => Action::Trigger,
            },
            created_at: op.created_at(),
            delivery: op.delivery,
            outcome,
            run_id: result.and_then(|r| r.run_id.clone()),
            error,
        })
    }

    fn invalidate_cache(state: &mut State) {
        state.cache.clear();
        state.cache_version += 1;
    }

    fn prune_cache(state: &mut State, now: Instant) {
        state
            .cache
            .retain(|_, entry| now.duration_since(entry.read_at) <= CACHE_RETAIN);
        while state.cache.len() > CACHE_ENTRIES {
            let Some(oldest) = state
                .cache
                .iter()
                .min_by_key(|(_, entry)| entry.used_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            state.cache.remove(&oldest);
        }
    }

    /// Retains a completed landing batch, evicting older batches that read
    /// any of the same channels (`retainDefinitions`).
    fn retain(&self, key: &str, channels: &[String], data: &WorkflowDefinitions, version: u64) {
        let mut state = self.lock();
        if version != state.cache_version {
            return;
        }
        let now = Instant::now();
        let requested: HashSet<&String> = channels.iter().collect();
        state
            .cache
            .retain(|_, entry| !entry.channels.iter().any(|id| requested.contains(id)));
        state.cache_serial += 1;
        let serial = state.cache_serial;
        state.cache.insert(
            key.to_string(),
            CacheEntry {
                data: data.clone(),
                channels: channels.to_vec(),
                read_at: now,
                used_at: now,
                serial,
            },
        );
        Self::prune_cache(&mut state, now);
    }

    fn assert_access(&self, reference: &WorkflowReference) -> Result<(), String> {
        protocol::validate_reference(reference)?;
        let state = self.lock();
        if state.closed || !state.access.contains(&reference.channel_id) {
            return Err("Workflow access unavailable; refresh channel membership".into());
        }
        Ok(())
    }

    fn send(
        &self,
        kind: u16,
        workflow: &WorkflowReference,
        yaml: String,
        revision: Option<&str>,
    ) -> Result<String, String> {
        self.assert_access(workflow)?;
        if workflow.owner != self.viewer {
            return Err("Only the workflow owner can modify or run it".into());
        }
        let mut tags = vec![vec!["h".to_string(), workflow.channel_id.clone()]];
        if kind == protocol::DELETION {
            tags.push(vec![
                "a".into(),
                format!("30620:{}:{}", workflow.owner, workflow.id),
            ]);
        } else {
            tags.push(vec!["d".into(), workflow.id.clone()]);
        }
        if let Some(revision) = revision {
            tags.push(vec!["expected-revision".into(), revision.to_string()]);
        }
        protocol::validate_command(
            &CommandView {
                kind,
                pubkey: &self.viewer,
                content: &yaml,
                tags: tags.iter().map(Vec::as_slice).collect(),
            },
            &self.viewer,
        )?;
        // Hold the lock across the send: its receipt may arrive at once and
        // must find the interest registered.
        let mut state = self.lock();
        if state.interest.len() >= MAX_OPERATIONS {
            return Err("Too many unresolved workflow commands".into());
        }
        let outbox = state
            .outbox
            .clone()
            .ok_or_else(|| "Workflow publishing unavailable".to_string())?;
        // Commands come from the UI thread; the outbox spawns on tokio.
        let _runtime = self.runtime.enter();
        let id = outbox
            .send(
                Draft {
                    kind,
                    content: yaml,
                    tags,
                },
                SendOptions::default(),
            )
            .map_err(|error| error.to_string())?
            .to_hex();
        if kind != protocol::TRIGGER {
            Self::invalidate_cache(&mut state);
        }
        state.interest.insert(id.clone());
        drop(state);
        self.rebuild();
        Ok(id)
    }

    /// A command's receipt (`receipt`): its relay message, or `None` when
    /// delivery could not be confirmed.
    fn receipt(&self, id: EventId, message: Option<String>) {
        let event_id = id.to_hex();
        let op = {
            let mut state = self.lock();
            if state.closed || !state.interest.remove(&event_id) {
                return;
            }
            let op = state.outbox.as_ref().and_then(|outbox| outbox.get(&id));
            if op.as_ref().is_some_and(|op| op.kind() != protocol::TRIGGER) {
                Self::invalidate_cache(&mut state);
            }
            if message.is_none() {
                if state.results.get(&event_id).and_then(|r| r.outcome) != Some(Outcome::Succeeded)
                {
                    state.results.remove(&event_id);
                }
                drop(state);
                self.rebuild();
                return;
            }
            op
        };
        let message = message.unwrap_or_default();
        let mut result = Result_ {
            outcome: Some(Outcome::Unknown),
            error: Some(UNKNOWN_RESULT.into()),
            ..Default::default()
        };
        if let Some(op) = op {
            let (kind, pubkey, tags) = command_view(&op);
            let view = CommandView {
                kind,
                pubkey: &pubkey,
                content: &op.event.content,
                tags,
            };
            if let Ok(reference) = protocol::reference(&view)
                && self.assert_access(&reference).is_ok()
            {
                if kind == protocol::DELETION && message.is_empty() {
                    result.deletion_accepted = true;
                }
                if let Some(json) = message.strip_prefix("response:")
                    && let Ok(Value::Object(value)) = serde_json::from_str::<Value>(json)
                {
                    let workflow_id = value.get("workflow_id");
                    let names = match workflow_id {
                        None => kind == protocol::TRIGGER,
                        Some(id) => id.as_str() == Some(reference.id.as_str()),
                    };
                    if names {
                        let secret = value.get("webhook_secret");
                        if kind == protocol::DEFINITION {
                            match secret {
                                None => result = succeeded(),
                                Some(Value::String(secret)) => {
                                    result = succeeded();
                                    self.lock().secrets.insert(event_id.clone(), secret.clone());
                                }
                                Some(_) => {}
                            }
                        } else if secret.is_some() {
                            // An unexpected secret on a run or deletion stays unknown.
                        } else if kind == protocol::TRIGGER
                            && let Some(run) = value
                                .get("run_id")
                                .and_then(Value::as_str)
                                .filter(|run| protocol::is_uuid(run))
                        {
                            result = Result_ {
                                run_id: Some(run.to_string()),
                                ..succeeded()
                            };
                        } else if kind == protocol::DELETION
                            && value.get("deleted") == Some(&Value::Bool(true))
                        {
                            result = Result_ {
                                deletion_accepted: true,
                                ..succeeded()
                            };
                        }
                    }
                }
            }
        }
        {
            let mut state = self.lock();
            // An unavailable or malformed receipt cannot undo verified readback.
            let verified =
                state.results.get(&event_id).and_then(|r| r.outcome) == Some(Outcome::Succeeded);
            if result.outcome == Some(Outcome::Succeeded) || !verified {
                state.results.insert(event_id, result);
            }
        }
        self.rebuild();
    }

    /// Resolves unknown saves and accepted deletions against a fresh read
    /// (`accept` of `definitions`).
    fn readback(
        &self,
        channels: &[String],
        data: &WorkflowDefinitions,
        accepted_deletions: &HashSet<String>,
    ) {
        let mut changed = false;
        {
            let mut state = self.lock();
            let operations = state.operations.clone();
            for op in operations.iter() {
                let present = |strict_revision: bool| {
                    data.items.iter().any(|row| {
                        row.names(&op.workflow) && (!strict_revision || row.revision == op.event_id)
                    })
                };
                if op.action == Action::Delete
                    && op.outcome == Outcome::Unknown
                    && accepted_deletions.contains(&op.event_id)
                    && channels.contains(&op.workflow.channel_id)
                    && !data.partial_channel_ids.contains(&op.workflow.channel_id)
                    && !present(false)
                {
                    state.results.insert(
                        op.event_id.clone(),
                        Result_ {
                            deletion_accepted: true,
                            ..succeeded()
                        },
                    );
                    changed = true;
                    continue;
                }
                if op.action != Action::Save || op.outcome != Outcome::Unknown || !present(true) {
                    continue;
                }
                // Exact readback proves the configuration, not delivery of
                // the one-time secret; the pending receipt still owns that.
                state.results.insert(op.event_id.clone(), succeeded());
                changed = true;
            }
        }
        if changed {
            self.rebuild();
        }
    }

    fn handles(&self) -> Vec<Arc<dyn Handle>> {
        self.lock()
            .views
            .values()
            .filter_map(Weak::upgrade)
            .collect()
    }
}

fn succeeded() -> Result_ {
    Result_ {
        outcome: Some(Outcome::Succeeded),
        ..Default::default()
    }
}

/// Reads one kind-30620 page for `channels`.
async fn read_page(
    reader: &Reader,
    channels: &[String],
    cursor: Option<(u64, String)>,
) -> Result<Vec<Verified>, String> {
    let mut filter = json!({
        "kinds": [protocol::DEFINITION],
        "#h": channels,
        "limit": protocol::DEFINITION_LIMIT,
    });
    if let Some((until, before_id)) = cursor {
        filter["until"] = json!(until);
        filter["before_id"] = json!(before_id);
    }
    let Value::Object(filter) = filter else {
        unreachable!("filters are objects")
    };
    let events = reader
        .read(
            vec![filter],
            ReadOptions {
                fresh: true,
                ..Default::default()
            },
        )
        .await
        .map_err(|error| error.message)?;
    Ok(events.to_vec())
}

impl Workflows {
    pub(crate) fn new(
        runtime: tokio::runtime::Handle,
        viewer: String,
        reader: Reader,
        http: RelayHttp,
    ) -> Self {
        let relay_http = http.origin().http();
        let inner = Arc::new_cyclic(|me| Inner {
            runtime,
            viewer,
            reader,
            http,
            relay_http,
            state: Mutex::new(State::default()),
            operations: watch::Sender::new(Arc::new([])),
            me: me.clone(),
        });
        Self { inner }
    }

    /// Host availability. This native host signs and reads every workflow
    /// operation; the relay decides per-row permission.
    pub fn availability(&self) -> Availability {
        Availability {
            definitions: true,
            history: true,
            save: true,
            trigger: true,
            delete: true,
        }
    }

    pub fn viewer(&self) -> &str {
        &self.inner.viewer
    }

    /// Wires the session outbox once it exists, and follows its changes.
    pub(crate) fn attach(&self, outbox: Outbox) {
        let mut changes = outbox.changes();
        self.inner.lock().outbox = Some(outbox);
        self.inner.rebuild();
        let weak = Arc::downgrade(&self.inner);
        self.inner.runtime.spawn(async move {
            while changes.changed().await.is_ok() {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                inner.rebuild();
            }
        });
    }

    /// The outbox receipt listener (`onReceipt`).
    pub(crate) fn receipt_listener(&self) -> crate::outbox::ReceiptListener {
        let weak = Arc::downgrade(&self.inner);
        Arc::new(move |id, message| {
            if let Some(inner) = weak.upgrade() {
                inner.receipt(id, message);
            }
        })
    }

    /// The channels the viewer may participate in (`canAccess`). Losing a
    /// channel withdraws its operations and its views' data.
    pub(crate) fn set_access(&self, channels: HashSet<String>) {
        let lost = {
            let mut state = self.inner.lock();
            if state.access == channels {
                return;
            }
            let lost = state.access.iter().any(|id| !channels.contains(id));
            state.access = channels;
            lost
        };
        if lost {
            for view in self.inner.handles() {
                view.clear();
            }
        }
        self.inner.rebuild();
    }

    /// Validates a command at the signing boundary (`validate`).
    pub(crate) fn validate(
        &self,
        kind: u16,
        pubkey: &str,
        content: &str,
        tags: &[&[String]],
    ) -> Result<(), String> {
        if !protocol::is_operation(kind, tags) {
            return Ok(());
        }
        let view = CommandView {
            kind,
            pubkey,
            content,
            tags: tags.to_vec(),
        };
        let reference = protocol::validate_command(&view, &self.inner.viewer)?;
        self.inner.assert_access(&reference)
    }

    /// Socket recovery retires reads, not drafts or receipt interest.
    pub(crate) fn interrupt(&self) {
        Inner::invalidate_cache(&mut self.inner.lock());
        for view in self.inner.handles() {
            view.interrupt();
        }
    }

    /// Access loss or cache clear purges copied data and results.
    pub(crate) fn clear(&self) {
        {
            let mut state = self.inner.lock();
            Inner::invalidate_cache(&mut state);
            state.results.clear();
            state.secrets.clear();
            state.interest.clear();
        }
        for view in self.inner.handles() {
            view.clear();
        }
        self.inner.rebuild();
    }

    pub(crate) fn dispose(&self) {
        {
            let mut state = self.inner.lock();
            state.closed = true;
            Inner::invalidate_cache(&mut state);
            state.results.clear();
            state.secrets.clear();
            state.interest.clear();
        }
        for view in self.inner.handles() {
            view.clear();
        }
        self.inner.rebuild();
    }

    pub fn operations(&self) -> Arc<[WorkflowOperation]> {
        self.inner.lock().operations.clone()
    }

    pub fn watch_operations(&self) -> watch::Receiver<Arc<[WorkflowOperation]>> {
        self.inner.operations.subscribe()
    }

    /// Definitions in one channel (bounded to 100 rows, saturated reads
    /// marked partial) or, for the landing, a batch of up to 128 channels
    /// paged to the end.
    pub fn definitions(
        &self,
        channels: Vec<String>,
        aggregate: bool,
    ) -> Result<Definitions, String> {
        let mut channels: Vec<String> = {
            let mut seen = HashSet::new();
            channels
                .into_iter()
                .filter(|id| seen.insert(id.clone()))
                .collect()
        };
        if channels.is_empty()
            || channels.len() > protocol::CHANNEL_BATCH
            || channels.iter().any(|id| !protocol::is_uuid(id))
            || (!aggregate && channels.len() != 1)
        {
            return Err("Invalid workflow channels".into());
        }
        let inner = self.inner.clone();
        let key = if aggregate {
            let mut sorted = channels.clone();
            sorted.sort();
            sorted.join(":")
        } else {
            String::new()
        };
        let cached = aggregate.then(|| {
            let mut state = inner.lock();
            let now = Instant::now();
            Inner::prune_cache(&mut state, now);
            state.cache.get_mut(&key).map(|entry| {
                entry.used_at = now;
                (entry.data.clone(), entry.serial)
            })
        });
        let version = Arc::new(Mutex::new(inner.lock().cache_version));
        let accepted: Arc<Mutex<HashSet<String>>> = Arc::default();
        let load = {
            let inner = inner.clone();
            let channels = channels.clone();
            let version = version.clone();
            let accepted = accepted.clone();
            Box::new(move || {
                let inner = inner.clone();
                let channels = channels.clone();
                {
                    let state = inner.lock();
                    *version.lock().expect("version") = state.cache_version;
                    // Only receipts from before this read can use absence
                    // as readback.
                    *accepted.lock().expect("accepted") = state
                        .operations
                        .iter()
                        .filter(|op| {
                            state
                                .results
                                .get(&op.event_id)
                                .is_some_and(|r| r.deletion_accepted)
                        })
                        .map(|op| op.event_id.clone())
                        .collect();
                }
                async move { load_definitions(&inner, &channels, aggregate).await }.boxed()
            }) as view::Loader<WorkflowDefinitions>
        };
        let accept = {
            let inner = inner.clone();
            let channels = channels.clone();
            let key = key.clone();
            Box::new(move |data: &WorkflowDefinitions| {
                if aggregate {
                    let version = *version.lock().expect("version");
                    inner.retain(&key, &channels, data, version);
                }
                let accepted = accepted.lock().expect("accepted").clone();
                inner.readback(&channels, data, &accepted);
            }) as view::Accept<WorkflowDefinitions>
        };
        let fresh_check = cached.flatten().map(|(data, serial)| {
            let inner = inner.clone();
            let key = key.clone();
            let check: view::Check = Box::new(move || {
                inner.lock().cache.get(&key).is_some_and(|entry| {
                    entry.serial == serial && entry.read_at.elapsed() < CACHE_FRESH
                })
            });
            (data, check)
        });
        let needs_fresh = {
            let inner = inner.clone();
            let channels = channels.clone();
            Box::new(move || {
                let state = inner.lock();
                state.operations.iter().any(|op| {
                    channels.contains(&op.workflow.channel_id)
                        && state
                            .results
                            .get(&op.event_id)
                            .is_some_and(|r| r.deletion_accepted)
                })
            }) as view::Check
        };
        channels.shrink_to_fit();
        self.view(Config {
            id: 0,
            channels,
            available: true,
            deadline: !aggregate,
            empty: WorkflowDefinitions::default(),
            load,
            accept: Some(accept),
            cached: fresh_check,
            needs_fresh: Some(needs_fresh),
            runtime: inner.runtime.clone(),
            owner: inner.me.clone() as Weak<dyn Owner>,
        })
    }

    /// One page of run history, newest first.
    pub fn runs(
        &self,
        workflow: &WorkflowDefinition,
        cursor: Option<RunCursor>,
    ) -> Result<Runs, String> {
        self.runs_of(workflow.reference(), cursor)
    }

    /// [`Self::runs`] by reference: the workflow need not be read first,
    /// as a deleted one's history still pages (`runs(workflow)`).
    pub fn runs_of(
        &self,
        reference: WorkflowReference,
        cursor: Option<RunCursor>,
    ) -> Result<Runs, String> {
        self.inner.assert_access(&reference)?;
        let inner = self.inner.clone();
        let id = reference.id.clone();
        let load = Box::new(move || {
            let http = inner.http.clone();
            let id = id.clone();
            let cursor = cursor.clone();
            async move {
                let path = protocol::runs_path(&id, cursor.as_ref())?;
                let body = http
                    .get(&path, protocol::READ_BYTES)
                    .await
                    .map_err(|error| error.to_string())?;
                let raw: Value = std::str::from_utf8(&body)
                    .ok()
                    .and_then(|text| serde_json::from_str(text).ok())
                    .ok_or_else(|| "Invalid workflow response".to_string())?;
                protocol::parse_runs(&raw, &id)
            }
            .boxed()
        }) as view::Loader<RunPage>;
        self.view(Config {
            id: 0,
            channels: vec![reference.channel_id],
            available: true,
            deadline: true,
            empty: RunPage::default(),
            load,
            accept: None,
            cached: None,
            needs_fresh: None,
            runtime: self.inner.runtime.clone(),
            owner: self.inner.me.clone() as Weak<dyn Owner>,
        })
    }

    fn view<T: Clone + Send + Sync + 'static>(
        &self,
        mut config: Config<T>,
    ) -> Result<WorkflowView<T>, String> {
        // Reserve the slot first so a refused view never reads.
        {
            let mut state = self.inner.lock();
            state.views.retain(|_, view| view.strong_count() > 0);
            if state.views.len() >= MAX_VIEWS {
                return Err("Too many workflow views; close another detail first".into());
            }
            state.next_view += 1;
            config.id = state.next_view;
        }
        let id = config.id;
        let (view, handle) = WorkflowView::new(config);
        // The registry holds only a weak handle; the view keeps it alive.
        self.inner.lock().views.insert(id, Arc::downgrade(&handle));
        Ok(view)
    }

    /// Saves a definition: an existing one keeps its owner, channel and
    /// UUID and signs its revision as `expected-revision`; a new one gets
    /// a new UUID. Returns the command's event ID.
    pub fn save(
        &self,
        channel: &str,
        yaml: String,
        existing: Option<&WorkflowDefinition>,
    ) -> Result<String, String> {
        if existing.is_some_and(|existing| existing.channel_id != channel) {
            return Err("Workflow channel cannot change".into());
        }
        let reference = existing
            .map(WorkflowDefinition::reference)
            .unwrap_or_else(|| WorkflowReference {
                id: uuid::Uuid::new_v4().to_string(),
                owner: self.inner.viewer.clone(),
                channel_id: channel.to_string(),
            });
        self.inner.send(
            protocol::DEFINITION,
            &reference,
            yaml,
            existing.map(|existing| existing.revision.as_str()),
        )
    }

    pub fn delete(&self, workflow: &WorkflowDefinition) -> Result<String, String> {
        self.inner.send(
            protocol::DELETION,
            &workflow.reference(),
            String::new(),
            None,
        )
    }

    pub fn trigger(&self, workflow: &WorkflowDefinition) -> Result<String, String> {
        self.inner.send(
            protocol::TRIGGER,
            &workflow.reference(),
            String::new(),
            None,
        )
    }

    /// Hands over the secret a succeeded save received, once.
    pub fn take_webhook_secret(&self, event_id: &str) -> Option<String> {
        let secret = self.inner.lock().secrets.remove(event_id)?;
        self.inner.rebuild();
        Some(secret)
    }

    /// The display-only hook address.
    pub fn webhook_url(&self, workflow_id: &str) -> Option<String> {
        protocol::hook_url(&self.inner.relay_http, workflow_id).ok()
    }

    /// Clears a notice once its removal is durable. Neither undoes nor
    /// repeats the command.
    pub fn dismiss(&self, event_id: &str) -> BoxFuture<'static, Result<(), String>> {
        let inner = self.inner.clone();
        let event_id = event_id.to_string();
        async move {
            let id = EventId::from_hex(&event_id).map_err(|_| "Unknown command".to_string())?;
            let outbox = inner.lock().outbox.clone();
            if let Some(outbox) = &outbox {
                outbox
                    .dismiss(&id)
                    .await
                    .map_err(|error| error.to_string())?;
                // The outbox cannot dismiss an active attempt, even one the
                // relay already echoed.
                if outbox.get(&id).is_some() {
                    return Err(
                        "Command is still being delivered; wait before dismissing it".into(),
                    );
                }
            }
            {
                let mut state = inner.lock();
                state.results.remove(&event_id);
                state.interest.remove(&event_id);
            }
            inner.rebuild();
            Ok(())
        }
        .boxed()
    }
}

/// Reads definitions, paging a landing batch to its end with the relay's
/// exact `until`/`before_id` cursor and failing a page that does not
/// advance.
async fn load_definitions(
    inner: &Inner,
    channels: &[String],
    aggregate: bool,
) -> Result<WorkflowDefinitions, String> {
    let accessible = || channels.iter().all(|id| inner.can_access(id));
    let mut coordinates: HashMap<(String, String, String), WorkflowDefinition> = HashMap::new();
    let mut order: Vec<(String, String, String)> = Vec::new();
    let mut cursor: Option<(u64, String)> = None;
    let mut partial = false;
    loop {
        if !accessible() {
            return Err("Workflow channel access unavailable".into());
        }
        let events = read_page(&inner.reader, channels, cursor.clone()).await?;
        if !accessible() {
            return Err("Workflow channel access unavailable".into());
        }
        let mut page: Vec<Verified> = {
            let mut seen = HashSet::new();
            events
                .into_iter()
                .filter(|event| seen.insert(event.id))
                .collect()
        };
        page.sort_by(|a, b| newest_first(a, b));
        for event in &page {
            if let Some((until, before_id)) = &cursor
                && (event.created_at_secs() > *until
                    || (event.created_at_secs() == *until && event.id_hex() <= *before_id))
            {
                return Err("Workflow page did not advance".into());
            }
            let row = protocol::definition(event)?;
            if !channels.contains(&row.channel_id) {
                return Err("Mismatched workflow channel".into());
            }
            let key = (row.channel_id.clone(), row.owner.clone(), row.id.clone());
            match coordinates.get(&key) {
                Some(old)
                    if !(row.created_at > old.created_at
                        || (row.created_at == old.created_at && row.revision < old.revision)) => {}
                existing => {
                    if existing.is_none() {
                        order.push(key.clone());
                    }
                    coordinates.insert(key, row);
                }
            }
        }
        let full = page.len() >= protocol::DEFINITION_LIMIT;
        if !aggregate {
            partial = full;
            break;
        }
        match page.last() {
            Some(last) if full => cursor = Some((last.created_at_secs(), last.id_hex())),
            _ => break,
        }
    }
    Ok(WorkflowDefinitions {
        items: order
            .into_iter()
            .filter_map(|key| coordinates.remove(&key))
            .collect(),
        partial,
        partial_channel_ids: if partial {
            channels.to_vec()
        } else {
            Vec::new()
        },
    })
}
