//! Janet's lanes for attention turns (buzz-app's `lanes.ts`): an
//! Interest's turns run one at a time, in order, and only a few Interests
//! run at once, so a burst of watch matches cannot take every process the
//! agent has. A slot is freed after each turn and goes to the Interest that
//! has waited longest, so a busy Interest cannot hold one while others
//! wait.

use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Interests whose turns run at once.
pub const LANES: usize = 2;
/// Turns one Interest can have waiting, and all Interests together, as in
/// Janet; later ones are dropped and logged.
pub const LANE_LIMIT: usize = 8;
pub const TOTAL_LIMIT: usize = 32;

/// Told when a waiting turn is dropped.
pub type Dropped = Box<dyn FnOnce() + Send>;

struct Queued {
    task: BoxFuture<'static, ()>,
    dropped: Option<Dropped>,
}

#[derive(Default)]
struct State {
    queues: HashMap<String, VecDeque<Queued>>,
    /// Lanes with work waiting for a free slot, longest waiting first.
    ready: VecDeque<String>,
    running: HashSet<String>,
    waiting: usize,
    stopped: bool,
}

/// What [`Lanes::add`] did with a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    Queued,
    Full,
    Stopped,
}

/// Cheap to clone; clones share the lanes. Turns run on tokio.
#[derive(Clone)]
pub struct Lanes {
    state: Arc<Mutex<State>>,
    width: usize,
}

impl Default for Lanes {
    fn default() -> Self {
        Self::new(LANES)
    }
}

impl Lanes {
    pub fn new(width: usize) -> Self {
        Self {
            state: Arc::default(),
            width,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `task` on `lane`, or says why it was not. `dropped` is
    /// called if the task is dropped while it waits.
    pub fn add(&self, lane: &str, task: BoxFuture<'static, ()>, dropped: Option<Dropped>) -> Added {
        {
            let mut state = self.state();
            if state.stopped {
                return Added::Stopped;
            }
            let length = state.queues.get(lane).map_or(0, VecDeque::len);
            if length >= LANE_LIMIT || state.waiting >= TOTAL_LIMIT {
                return Added::Full;
            }
            state
                .queues
                .entry(lane.to_owned())
                .or_default()
                .push_back(Queued { task, dropped });
            state.waiting += 1;
            if !state.running.contains(lane) && !state.ready.iter().any(|other| other == lane) {
                state.ready.push_back(lane.to_owned());
            }
        }
        self.pump();
        Added::Queued
    }

    /// Drops every waiting turn; running ones finish.
    pub fn clear(&self) {
        let queues: Vec<VecDeque<Queued>> = {
            let mut state = self.state();
            state.ready.clear();
            state.waiting = 0;
            state.queues.drain().map(|(_, queue)| queue).collect()
        };
        for queued in queues.into_iter().flatten() {
            if let Some(dropped) = queued.dropped {
                dropped();
            }
        }
    }

    /// Drops every waiting turn, and takes no more.
    pub fn stop(&self) {
        self.state().stopped = true;
        self.clear();
    }

    fn pump(&self) {
        loop {
            let (lane, task) = {
                let mut state = self.state();
                if state.running.len() >= self.width {
                    return;
                }
                let Some(lane) = state.ready.pop_front() else {
                    return;
                };
                let Some(queued) = state.queues.get_mut(&lane).and_then(VecDeque::pop_front) else {
                    state.queues.remove(&lane);
                    continue;
                };
                state.waiting -= 1;
                state.running.insert(lane.clone());
                (lane, queued.task)
            };
            let this = self.clone();
            tokio::spawn(async move {
                task.await;
                {
                    let mut state = this.state();
                    state.running.remove(&lane);
                    if state
                        .queues
                        .get(&lane)
                        .is_some_and(|queue| !queue.is_empty())
                    {
                        state.ready.push_back(lane);
                    } else {
                        state.queues.remove(&lane);
                    }
                }
                this.pump();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use tokio::sync::oneshot;

    /// A turn that records it started and waits to be let go.
    fn turn(
        started: &Arc<Mutex<Vec<String>>>,
        name: &str,
    ) -> (BoxFuture<'static, ()>, oneshot::Sender<()>) {
        let (release, wait) = oneshot::channel();
        let (started, name) = (started.clone(), name.to_owned());
        let task = async move {
            started.lock().unwrap().push(name);
            let _ = wait.await;
        }
        .boxed();
        (task, release)
    }

    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn runs_one_turn_at_a_time_per_interest_two_at_once_and_frees_a_slot_after_each() {
        let lanes = Lanes::default();
        let started = Arc::new(Mutex::new(Vec::new()));
        let mut releases = HashMap::new();
        for (lane, name) in [("a", "a1"), ("a", "a2"), ("b", "b1"), ("c", "c1")] {
            let (task, release) = turn(&started, name);
            assert_eq!(lanes.add(lane, task, None), Added::Queued);
            releases.insert(name, release);
        }
        settle().await;
        assert_eq!(*started.lock().unwrap(), ["a1", "b1"]);
        // a's slot goes to c, which waited longer than a's second turn.
        let _ = releases.remove("a1").unwrap().send(());
        settle().await;
        assert_eq!(*started.lock().unwrap(), ["a1", "b1", "c1"]);
        let _ = releases.remove("b1").unwrap().send(());
        settle().await;
        assert_eq!(*started.lock().unwrap(), ["a1", "b1", "c1", "a2"]);
    }

    #[tokio::test]
    async fn drops_turns_over_its_limits_and_on_clear_or_stop() {
        let lanes = Lanes::new(0);
        let dropped = Arc::new(Mutex::new(0));
        let counted = || -> Option<Dropped> {
            let dropped = dropped.clone();
            Some(Box::new(move || *dropped.lock().unwrap() += 1))
        };
        for _ in 0..LANE_LIMIT {
            assert_eq!(lanes.add("a", async {}.boxed(), counted()), Added::Queued);
        }
        assert_eq!(lanes.add("a", async {}.boxed(), None), Added::Full);
        lanes.clear();
        assert_eq!(*dropped.lock().unwrap(), LANE_LIMIT);
        for index in 0..TOTAL_LIMIT {
            let lane = format!("l{}", index % 8);
            assert_eq!(lanes.add(&lane, async {}.boxed(), None), Added::Queued);
        }
        assert_eq!(lanes.add("z", async {}.boxed(), None), Added::Full);
        lanes.stop();
        assert_eq!(lanes.add("z", async {}.boxed(), None), Added::Stopped);
    }
}
