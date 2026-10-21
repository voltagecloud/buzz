//! Landing discovery (`useLandingDefinitions` in `WorkflowLanding.tsx`):
//! serial batches of up to 128 participating channels, one status for the
//! whole scan, a pause and Retry that keep what loaded, and readback
//! refreshes for the channels a command touched.
//!
//! At most two capability views stay live: the batch being read, and one
//! completed batch kept to observe a session clear or access loss.

use super::capability::{Definitions, Workflows};
use super::protocol::CHANNEL_BATCH;
use super::types::WorkflowDefinitions;
use super::view::{ViewSnapshot, ViewStatus};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::watch;
use tokio::task::AbortHandle;

pub const FAILURE: &str = "Workflow read unavailable. Retry; this is not proof of deletion.";

/// What the landing shows: each channel's definitions and whether the
/// scan paused on a failure.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LandingSnapshot {
    pub snapshots: HashMap<String, ViewSnapshot<WorkflowDefinitions>>,
    pub paused: bool,
}

struct Read {
    serial: u64,
    ids: Vec<String>,
    if_stale: bool,
    view: Definitions,
    forward: AbortHandle,
}

#[derive(Default)]
struct State {
    snapshots: HashMap<String, ViewSnapshot<WorkflowDefinitions>>,
    paused: bool,
    channels: Vec<String>,
    /// Channels still to read, and whether retained data may satisfy them.
    queued: HashMap<String, bool>,
    active: Option<Read>,
    retained: Option<Read>,
    invalidated: bool,
    next: u64,
    disposed: bool,
}

struct Inner {
    capability: Workflows,
    runtime: tokio::runtime::Handle,
    state: Mutex<State>,
    out: watch::Sender<Arc<LandingSnapshot>>,
    me: Weak<Inner>,
}

/// One landing's discovery. Dropping it releases its views.
pub struct Landing {
    inner: Arc<Inner>,
}

impl Drop for Landing {
    fn drop(&mut self) {
        self.dispose();
    }
}

fn empty() -> WorkflowDefinitions {
    WorkflowDefinitions::default()
}

fn has_copied(data: &WorkflowDefinitions) -> bool {
    !data.items.is_empty() || data.partial || !data.partial_channel_ids.is_empty()
}

fn definitions_for(id: &str, data: &WorkflowDefinitions) -> WorkflowDefinitions {
    WorkflowDefinitions {
        items: data
            .items
            .iter()
            .filter(|row| row.channel_id == id)
            .cloned()
            .collect(),
        partial: data.partial_channel_ids.iter().any(|channel| channel == id),
        partial_channel_ids: Vec::new(),
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("landing lock")
    }

    fn emit(&self, state: &State) {
        self.out.send_replace(Arc::new(LandingSnapshot {
            snapshots: state.snapshots.clone(),
            paused: state.paused,
        }));
    }

    fn release(read: Option<Read>) {
        if let Some(read) = read {
            read.forward.abort();
        }
    }

    fn failure(state: &State, id: &str) -> ViewSnapshot<WorkflowDefinitions> {
        ViewSnapshot {
            status: ViewStatus::Error,
            data: state
                .snapshots
                .get(id)
                .map(|snapshot| snapshot.data.clone())
                .unwrap_or_default(),
            error: Some(FAILURE.into()),
        }
    }

    fn record(state: &mut State, ids: &[String], snapshot: &ViewSnapshot<WorkflowDefinitions>) {
        for id in ids {
            let previous = state.snapshots.get(id).map(|s| s.data.clone());
            let keep = matches!(snapshot.status, ViewStatus::Loading | ViewStatus::Error)
                && previous.as_ref().is_some_and(has_copied);
            let data = if keep {
                previous.unwrap_or_default()
            } else {
                definitions_for(id, &snapshot.data)
            };
            state.snapshots.insert(
                id.clone(),
                ViewSnapshot {
                    status: snapshot.status,
                    data,
                    error: snapshot.error.clone(),
                },
            );
        }
    }

    fn purge(&self, state: &mut State, snapshot: &ViewSnapshot<WorkflowDefinitions>) {
        state.invalidated = true;
        state.paused = false;
        state.queued.clear();
        Self::release(state.active.take());
        state.snapshots = state
            .channels
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    ViewSnapshot {
                        status: snapshot.status,
                        data: empty(),
                        error: snapshot.error.clone(),
                    },
                )
            })
            .collect();
        self.emit(state);
    }

    fn pause(&self, state: &mut State, serial: u64, snapshot: &ViewSnapshot<WorkflowDefinitions>) {
        state.paused = true;
        let owned = if state.active.as_ref().is_some_and(|r| r.serial == serial) {
            state.active.take()
        } else if state.retained.as_ref().is_some_and(|r| r.serial == serial) {
            state.retained.take()
        } else {
            None
        };
        let Some(owned) = owned else {
            return;
        };
        for id in &owned.ids {
            state.queued.insert(id.clone(), false);
        }
        if let Some(active) = state.active.take() {
            for id in &active.ids {
                state.queued.insert(id.clone(), false);
            }
            Self::release(Some(active));
        }
        Self::release(state.retained.take());
        Self::record(state, &owned.ids, snapshot);
        state.retained = Some(owned);
        self.emit(state);
    }

    /// A view's snapshot changed.
    fn changed(&self, serial: u64) {
        let mut state = self.lock();
        let view = [&state.active, &state.retained]
            .into_iter()
            .flatten()
            .find(|read| read.serial == serial)
            .map(|read| (read.view.snapshot(), read.ids.clone()));
        let Some((snapshot, ids)) = view else {
            return;
        };
        match snapshot.status {
            ViewStatus::Idle | ViewStatus::Unavailable => self.purge(&mut state, &snapshot),
            ViewStatus::Error => self.pause(&mut state, serial, &snapshot),
            _ => {
                Self::record(&mut state, &ids, &snapshot);
                self.emit(&state);
            }
        }
    }

    fn observe(&self, state: &mut State, ids: Vec<String>, if_stale: bool) -> Result<Read, String> {
        let view = self.capability.definitions(ids.clone(), true)?;
        state.next += 1;
        let serial = state.next;
        let mut changes = view.watch();
        let weak = self.me.clone();
        let forward = self.runtime.spawn(async move {
            while changes.changed().await.is_ok() {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                inner.changed(serial);
            }
        });
        Ok(Read {
            serial,
            ids,
            if_stale,
            view,
            forward: forward.abort_handle(),
        })
    }

    fn read_next(&self, state: &mut State) {
        if state.active.is_some() || state.paused || state.disposed {
            return;
        }
        let ids: Vec<String> = state
            .channels
            .iter()
            .filter(|id| state.queued.contains_key(*id))
            .take(CHANNEL_BATCH)
            .cloned()
            .collect();
        if ids.is_empty() {
            return;
        }
        let if_stale = ids.iter().all(|id| state.queued.get(id) == Some(&true));
        for id in &ids {
            state.queued.remove(id);
        }
        match self.observe(state, ids.clone(), if_stale) {
            Ok(read) => {
                let serial = read.serial;
                let done = read.view.refresh(read.if_stale);
                state.active = Some(read);
                let weak = self.me.clone();
                self.runtime.spawn(async move {
                    done.await;
                    if let Some(inner) = weak.upgrade() {
                        inner.settled(serial);
                    }
                });
            }
            Err(_) => {
                for id in &ids {
                    let failure = Self::failure(state, id);
                    state.snapshots.insert(id.clone(), failure);
                    state.queued.insert(id.clone(), false);
                }
                state.paused = true;
                self.emit(state);
            }
        }
    }

    /// The active batch's read finished.
    fn settled(&self, serial: u64) {
        let mut state = self.lock();
        if !state.active.as_ref().is_some_and(|r| r.serial == serial) {
            return;
        }
        let snapshot = state.active.as_ref().expect("active").view.snapshot();
        if snapshot.status == ViewStatus::Error {
            self.pause(&mut state, serial, &snapshot);
            return;
        }
        let owned = state.active.take().expect("active");
        Self::record(&mut state, &owned.ids, &snapshot);
        Self::release(state.retained.take());
        state.retained = Some(owned);
        self.emit(&state);
        self.read_next(&mut state);
    }
}

impl Landing {
    pub fn new(capability: Workflows, runtime: tokio::runtime::Handle) -> Self {
        Self {
            inner: Arc::new_cyclic(|me| Inner {
                capability,
                runtime,
                state: Mutex::new(State::default()),
                out: watch::Sender::new(Arc::default()),
                me: me.clone(),
            }),
        }
    }

    pub fn snapshot(&self) -> Arc<LandingSnapshot> {
        self.inner.out.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<Arc<LandingSnapshot>> {
        self.inner.out.subscribe()
    }

    /// The participating channels. New ones are queued; a membership change
    /// after a purge re-establishes interest. A session clear alone never
    /// restarts discovery.
    pub fn channels(&self, ids: Vec<String>) {
        let inner = &self.inner;
        let mut state = inner.lock();
        if state.invalidated {
            state.snapshots.clear();
        }
        state.invalidated = false;
        state.channels = ids.clone();
        let wanted: std::collections::HashSet<&String> = ids.iter().collect();
        if state
            .active
            .as_ref()
            .is_some_and(|read| read.ids.iter().any(|id| !wanted.contains(id)))
        {
            let previous = state.active.take().expect("active");
            for id in &previous.ids {
                if wanted.contains(id) {
                    state.queued.insert(id.clone(), previous.if_stale);
                }
            }
            Inner::release(Some(previous));
        }
        if state
            .retained
            .as_ref()
            .is_some_and(|read| read.ids.iter().any(|id| !wanted.contains(id)))
        {
            Inner::release(state.retained.take());
            // Keep copied snapshots subscribed to session invalidation
            // without rereading a completed channel.
            if let Some(first) = ids.first()
                && state.active.is_none()
                && let Ok(read) = inner.observe(&mut state, vec![first.clone()], true)
            {
                state.retained = Some(read);
            }
        }
        state.queued.retain(|id, _| wanted.contains(id));
        state.snapshots.retain(|id, _| wanted.contains(id));
        for id in &ids {
            if !state.snapshots.contains_key(id) {
                state.snapshots.insert(
                    id.clone(),
                    ViewSnapshot {
                        status: ViewStatus::Loading,
                        data: empty(),
                        error: None,
                    },
                );
                state.queued.insert(id.clone(), true);
            }
        }
        inner.emit(&state);
        inner.read_next(&mut state);
    }

    /// Resumes a paused scan with the failed and unread channels.
    pub fn retry(&self) {
        let mut state = self.inner.lock();
        state.paused = false;
        self.inner.read_next(&mut state);
        self.inner.emit(&state);
    }

    /// Rereads `ids` (all channels when `None`). A pending read already
    /// satisfies a refresh, so it is never cancelled and repeated;
    /// `after_pending` readback leaves a pause in place.
    pub fn refresh(&self, ids: Option<Vec<String>>, after_pending: bool) {
        let mut state = self.inner.lock();
        let ids = ids.unwrap_or_else(|| state.channels.clone());
        state.invalidated = false;
        if !after_pending {
            state.paused = false;
        }
        for id in ids {
            if state.channels.contains(&id) {
                state.queued.insert(id, false);
            }
        }
        if !after_pending && let Some(active) = &state.active {
            let active_ids = active.ids.clone();
            for id in active_ids {
                state.queued.remove(&id);
            }
        }
        self.inner.read_next(&mut state);
        self.inner.emit(&state);
    }

    pub fn dispose(&self) {
        let mut state = self.inner.lock();
        state.disposed = true;
        state.queued.clear();
        Inner::release(state.active.take());
        Inner::release(state.retained.take());
        state.snapshots.clear();
        state.paused = false;
    }
}
