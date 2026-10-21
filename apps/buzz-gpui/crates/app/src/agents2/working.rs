//! How long a run counts as working (buzz-app's `working` in
//! `service.ts`), and what shows it meanwhile: the request's local work
//! row (`local-activity.ts`), and the agent typing in the mention's
//! thread to everyone (`typing.rs`).

use super::attention::EventData;
use crate::agent_activity::local::{WorkHandle, WorkOutcome};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

pub(crate) type End = Box<dyn FnOnce() + Send>;

struct State {
    /// Work still holding the run: the run itself and what it handed off.
    /// Below zero once stopped.
    holds: i64,
    /// The first failure any hold reported.
    outcome: WorkOutcome,
    ends: Vec<End>,
    /// Started on the first hand-off, then ended with the rest.
    begin: Option<Box<dyn FnOnce() -> End + Send>>,
}

/// Keeps a run counted as working until it and everything it handed to
/// [`Working::until`] have settled. Cheap to clone; clones are one hold.
#[derive(Clone)]
pub struct Working {
    state: Arc<Mutex<State>>,
    /// The mention's local work row, if the run has one.
    work: Option<WorkHandle>,
    runtime: tokio::runtime::Handle,
}

impl Working {
    /// A hold held by the run itself, which ends what `ends` started once
    /// every hold is released, however long that takes: the harness, not
    /// Buzz, decides how long an agent works. Its work row is working from
    /// now on, and settles with the outcome the holds report.
    pub(crate) fn new(
        ends: Vec<End>,
        work: Option<WorkHandle>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        if let Some(work) = &work {
            work.started();
        }
        Self {
            state: Arc::new(Mutex::new(State {
                holds: 1,
                outcome: WorkOutcome::ok(),
                ends,
                begin: None,
            })),
            work,
            runtime,
        }
    }

    /// The run's local work row, if it has one.
    pub fn work(&self) -> Option<WorkHandle> {
        self.work.clone()
    }

    /// Starts `begin` when the run first hands work off with
    /// [`Working::until`], and ends what it started with the rest: a
    /// runtime that ignores the mention never shows it.
    pub(crate) fn on_first_hold(self, begin: impl FnOnce() -> End + Send + 'static) -> Self {
        self.lock().begin = Some(Box::new(begin));
        self
    }

    /// A run with nothing to show.
    pub fn none(runtime: tokio::runtime::Handle) -> Self {
        Self::new(Vec::new(), None, runtime)
    }

    /// Keeps the run counted as working after `run` returns, until `done`
    /// settles, for a type that hands its work off and returns early. A
    /// failed outcome shows the request failed.
    pub fn until(&self, done: impl std::future::Future<Output = WorkOutcome> + Send + 'static) {
        let begin = {
            let mut state = self.lock();
            if state.holds <= 0 {
                return;
            }
            state.holds += 1;
            state.begin.take()
        };
        if let Some(begin) = begin {
            let end = begin();
            let mut state = self.lock();
            if state.holds < 0 {
                drop(state);
                end();
            } else {
                state.ends.push(end);
            }
        }
        let working = self.clone();
        self.runtime.spawn(async move {
            let outcome = done.await;
            working.release_with(outcome);
        });
    }

    /// Counts the run in `working` until it stops, so a change of type
    /// waits out work it handed off too.
    pub(crate) fn counted(self, working: Arc<AtomicUsize>) -> Self {
        {
            let mut state = self.lock();
            if state.holds > 0 {
                working.fetch_add(1, Ordering::SeqCst);
                state.ends.push(Box::new(move || {
                    working.fetch_sub(1, Ordering::SeqCst);
                }));
            }
        }
        self
    }

    /// Lets go of one hold.
    pub(crate) fn release(&self) {
        self.release_with(WorkOutcome::ok());
    }

    /// Lets go of one hold that ended with `outcome`.
    pub(crate) fn release_with(&self, outcome: WorkOutcome) {
        let ended = {
            let mut state = self.lock();
            if state.holds <= 0 {
                return;
            }
            if !outcome.ok && state.outcome.ok {
                state.outcome = WorkOutcome::failed(
                    outcome
                        .error
                        .unwrap_or_else(|| "Could not finish this request".into()),
                );
            }
            state.holds -= 1;
            (state.holds == 0).then(|| state.outcome.clone())
        };
        if let Some(outcome) = ended {
            if let Some(work) = &self.work {
                work.finish(outcome);
            }
            self.stop();
        }
    }

    /// The agent went away mid-run: nothing is left to show.
    pub(crate) fn abort(&self) {
        if let Some(work) = &self.work {
            work.remove();
        }
        self.stop();
    }

    fn stop(&self) {
        let ends = {
            let mut state = self.lock();
            if state.holds < 0 {
                return;
            }
            state.holds = -1;
            std::mem::take(&mut state.ends)
        };
        for end in ends {
            end();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The thread an agent answering `event` works in: its root, or the event
/// itself when it starts one (NIP-10, the last marked tags winning).
pub(crate) fn thread_root(event: &EventData) -> String {
    let (mut root, mut reply) = (None, None);
    for tag in &event.tags {
        if tag.first().map(String::as_str) != Some("e") {
            continue;
        }
        let Some(id) = tag.get(1).filter(|id| !id.is_empty()) else {
            continue;
        };
        match tag.get(3).map(String::as_str) {
            Some("root") => root = Some(id.clone()),
            Some("reply") => reply = Some(id.clone()),
            _ => {}
        }
    }
    match reply {
        Some(reply) => root.unwrap_or(reply),
        None => event.id.clone(),
    }
}

#[cfg(test)]
mod tests;
