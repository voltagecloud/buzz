//! A read-interest view (`WorkflowView` in `capability.ts`): one bounded
//! read with a snapshot the UI watches. Dropping the view releases the
//! interest and cancels its read.

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{oneshot, watch};
use tokio::task::AbortHandle;

pub const READ_ERROR: &str = "Couldn't read workflow data. Try again.";
pub const INTERRUPTED: &str =
    "Connection interrupted. Refresh to check current workflow data; your draft is retained.";
/// Single-channel reads and run pages share one 10 s deadline.
const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewStatus {
    Idle,
    Loading,
    Ready,
    Error,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ViewSnapshot<T> {
    pub status: ViewStatus,
    pub data: T,
    pub error: Option<String>,
}

impl<T> ViewSnapshot<T> {
    fn new(status: ViewStatus, data: T) -> Self {
        Self {
            status,
            data,
            error: None,
        }
    }
}

pub type Loader<T> = Box<dyn Fn() -> BoxFuture<'static, Result<T, String>> + Send + Sync>;
pub type Accept<T> = Box<dyn Fn(&T) + Send + Sync>;
pub type Check = Box<dyn Fn() -> bool + Send + Sync>;
pub type Done = Shared<BoxFuture<'static, ()>>;

/// What the capability lends a view: whether it may read now.
pub(super) trait Owner: Send + Sync {
    fn closed(&self) -> bool;
    fn can_access(&self, channel: &str) -> bool;
    fn release(&self, view: u64);
}

pub(super) struct Config<T> {
    pub id: u64,
    pub channels: Vec<String>,
    pub available: bool,
    /// Single-channel and history reads carry their own deadline; paged
    /// batches use the reader's per-request one.
    pub deadline: bool,
    pub empty: T,
    pub load: Loader<T>,
    pub accept: Option<Accept<T>>,
    /// Retained data to start from, and whether it is still fresh.
    pub cached: Option<(T, Check)>,
    /// A pending read is too old to satisfy a refresh.
    pub needs_fresh: Option<Check>,
    pub runtime: tokio::runtime::Handle,
    pub owner: Weak<dyn Owner>,
}

struct State {
    disposed: bool,
    generation: u64,
    running: Option<(u64, AbortHandle)>,
    pending: Option<Done>,
}

pub(super) struct Inner<T> {
    config: Config<T>,
    snapshot: watch::Sender<Arc<ViewSnapshot<T>>>,
    state: Mutex<State>,
}

/// One view. Clones share it; dropping the last handle disposes it.
pub struct WorkflowView<T: Clone + Send + Sync + 'static> {
    inner: Arc<Inner<T>>,
    _release: Arc<Release<T>>,
}

impl<T: Clone + Send + Sync + 'static> Clone for WorkflowView<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _release: self._release.clone(),
        }
    }
}

struct Release<T: Clone + Send + Sync + 'static>(Arc<Inner<T>>);

impl<T: Clone + Send + Sync + 'static> Drop for Release<T> {
    fn drop(&mut self) {
        self.0.dispose();
    }
}

/// What the capability keeps of a view: enough to interrupt or clear it.
pub(super) trait Handle: Send + Sync {
    fn clear(&self);
    fn interrupt(&self);
}

impl<T: Clone + Send + Sync + 'static> Inner<T> {
    fn owner(&self) -> Option<Arc<dyn Owner>> {
        self.config.owner.upgrade()
    }

    fn closed(&self) -> bool {
        self.owner().is_none_or(|owner| owner.closed())
    }

    fn accessible(&self) -> bool {
        self.owner().is_some_and(|owner| {
            self.config
                .channels
                .iter()
                .all(|channel| owner.can_access(channel))
        })
    }

    fn set(&self, snapshot: ViewSnapshot<T>) {
        self.snapshot.send_replace(Arc::new(snapshot));
    }

    fn stop(state: &mut State) {
        if let Some((_, task)) = state.running.take() {
            task.abort();
        }
        state.pending = None;
    }

    fn clear_state(&self, state: &mut State) {
        Self::stop(state);
        let status =
            if self.config.available && !self.closed() && !state.disposed && self.accessible() {
                ViewStatus::Idle
            } else {
                ViewStatus::Unavailable
            };
        self.set(ViewSnapshot::new(status, self.config.empty.clone()));
    }

    fn dispose(&self) {
        {
            let mut state = self.state.lock().expect("view lock");
            if state.disposed {
                return;
            }
            state.disposed = true;
            self.clear_state(&mut state);
        }
        if let Some(owner) = self.owner() {
            owner.release(self.config.id);
        }
    }

    fn refresh(self: &Arc<Self>, if_stale: bool) -> Done {
        let ready = || futures::future::ready(()).boxed().shared();
        let mut state = self.state.lock().expect("view lock");
        if self.closed() || state.disposed || !self.config.available {
            return ready();
        }
        if !self.accessible() {
            self.clear_state(&mut state);
            return ready();
        }
        if let Some(pending) = state.pending.clone() {
            let stale = self
                .config
                .needs_fresh
                .as_ref()
                .is_some_and(|check| check());
            if !stale {
                return pending;
            }
            let generation = state.generation;
            let this = self.clone();
            return async move {
                pending.await;
                let current = this.state.lock().expect("view lock").generation;
                // An interrupted read is not followed up here.
                if current == generation {
                    this.refresh(false).await;
                }
            }
            .boxed()
            .shared();
        }
        let current = self.snapshot.borrow().clone();
        if if_stale
            && current.status == ViewStatus::Ready
            && self
                .config
                .cached
                .as_ref()
                .is_some_and(|(_, fresh)| fresh())
        {
            return ready();
        }
        state.generation += 1;
        let generation = state.generation;
        self.set(ViewSnapshot::new(ViewStatus::Loading, current.data.clone()));
        let (done, finished) = oneshot::channel::<()>();
        let this = self.clone();
        let read = (self.config.load)();
        let deadline = self.config.deadline;
        let task = self.config.runtime.spawn(async move {
            let result = if deadline {
                tokio::time::timeout(DEADLINE, read)
                    .await
                    .unwrap_or_else(|_| Err("Workflow read timed out".into()))
            } else {
                read.await
            };
            this.finish(generation, result);
            let _ = done.send(());
        });
        state.running = Some((generation, task.abort_handle()));
        let weak = Arc::downgrade(self);
        let pending = async move {
            // A read that died without finishing must not stay pending.
            if finished.await.is_err()
                && let Some(this) = weak.upgrade()
            {
                this.finish(generation, Err("Workflow read stopped".into()));
            }
        }
        .boxed()
        .shared();
        state.pending = Some(pending.clone());
        pending
    }

    fn finish(&self, generation: u64, result: Result<T, String>) {
        let accepted = {
            let mut state = self.state.lock().expect("view lock");
            let current = state
                .running
                .as_ref()
                .is_some_and(|(g, _)| *g == generation);
            if !current || state.disposed || self.closed() {
                return;
            }
            state.running = None;
            state.pending = None;
            match result {
                Ok(data) if self.accessible() => {
                    self.set(ViewSnapshot::new(ViewStatus::Ready, data.clone()));
                    Some(data)
                }
                Ok(_) => None,
                Err(error) => {
                    // Shown only as the generic read error; the cause is for logs.
                    tracing::debug!(%error, "workflow read failed");
                    self.set(ViewSnapshot {
                        status: ViewStatus::Error,
                        data: self.config.empty.clone(),
                        error: Some(READ_ERROR.into()),
                    });
                    None
                }
            }
        };
        if let (Some(data), Some(accept)) = (accepted, &self.config.accept) {
            accept(&data);
        }
    }
}

impl<T: Clone + Send + Sync + 'static> Handle for Inner<T> {
    fn clear(&self) {
        let mut state = self.state.lock().expect("view lock");
        self.clear_state(&mut state);
    }

    fn interrupt(&self) {
        let mut state = self.state.lock().expect("view lock");
        Self::stop(&mut state);
        // Even an idle view tells interruption (keep copied data) from
        // clear or access loss (purge it).
        let current = self.snapshot.borrow().clone();
        if current.status == ViewStatus::Unavailable {
            return;
        }
        self.set(ViewSnapshot {
            status: ViewStatus::Error,
            data: current.data.clone(),
            error: Some(INTERRUPTED.into()),
        });
    }
}

impl<T: Clone + Send + Sync + 'static> WorkflowView<T> {
    pub(super) fn new(config: Config<T>) -> (Self, Arc<dyn Handle>) {
        let usable = config.available
            && config.owner.upgrade().is_some_and(|owner| {
                !owner.closed() && config.channels.iter().all(|c| owner.can_access(c))
            });
        let initial = match (&config.cached, usable) {
            (Some((data, _)), true) => ViewSnapshot::new(ViewStatus::Ready, data.clone()),
            (None, true) => ViewSnapshot::new(ViewStatus::Idle, config.empty.clone()),
            (_, false) => ViewSnapshot::new(ViewStatus::Unavailable, config.empty.clone()),
        };
        let inner = Arc::new(Inner {
            snapshot: watch::Sender::new(Arc::new(initial)),
            state: Mutex::new(State {
                disposed: false,
                generation: 0,
                running: None,
                pending: None,
            }),
            config,
        });
        let handle: Arc<dyn Handle> = inner.clone();
        (
            Self {
                _release: Arc::new(Release(inner.clone())),
                inner,
            },
            handle,
        )
    }

    pub fn snapshot(&self) -> Arc<ViewSnapshot<T>> {
        self.inner.snapshot.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<Arc<ViewSnapshot<T>>> {
        self.inner.snapshot.subscribe()
    }

    /// Reads now, or joins the read in flight. `if_stale` keeps fresh
    /// retained data instead. The future resolves when the read settles.
    pub fn refresh(&self, if_stale: bool) -> Done {
        self.inner.refresh(if_stale)
    }

    /// The channels this view reads.
    pub fn channels(&self) -> &[String] {
        &self.inner.config.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Open;

    impl Owner for Open {
        fn closed(&self) -> bool {
            false
        }
        fn can_access(&self, _: &str) -> bool {
            true
        }
        fn release(&self, _: u64) {}
    }

    #[tokio::test]
    async fn a_read_that_dies_does_not_stay_pending() {
        let owner: Arc<dyn Owner> = Arc::new(Open);
        let first = AtomicBool::new(false);
        let (view, _handle) = WorkflowView::new(Config {
            id: 1,
            channels: Vec::new(),
            available: true,
            deadline: false,
            empty: 0u32,
            load: Box::new(move || {
                let die = !first.swap(true, Ordering::SeqCst);
                async move {
                    assert!(!die, "the read dies");
                    Ok(7)
                }
                .boxed()
            }),
            accept: None,
            cached: None,
            needs_fresh: None,
            runtime: tokio::runtime::Handle::current(),
            owner: Arc::downgrade(&owner),
        });
        view.refresh(false).await;
        assert_eq!(view.snapshot().status, ViewStatus::Error);
        view.refresh(false).await;
        assert_eq!(*view.snapshot(), ViewSnapshot::new(ViewStatus::Ready, 7));
    }
}
