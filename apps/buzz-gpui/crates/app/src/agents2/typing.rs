//! While an agent this app runs works on a mention, it shows as typing
//! there to everyone in the channel, on every device: the owner's others
//! and teammates' (buzz-app's `agents/typing.ts`). On this device the
//! agent's activity takes the place of its typing.

use super::attention::EventData;
use super::working::{End, thread_root};
use buzz_host::api::AppAgentEvent;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::oneshot;

/// Clients show a pulse for 8 s, so one every 3 s keeps it steady.
pub(crate) const PULSE: Duration = Duration::from_secs(3);
/// A run that never settles stops looking busy after this long.
const LONGEST: Duration = Duration::from_secs(30 * 60);
/// Failed pulses in a row that mean the community will not take them.
const FAILURES: usize = 3;

/// Signs and posts one event as the agent.
pub(crate) type Publish =
    Arc<dyn Fn(AppAgentEvent) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// One conversation's pulse, shared by the turns typing there.
struct Pulse {
    id: u64,
    turns: usize,
    stop: Option<oneshot::Sender<()>>,
}

/// One agent's pulses by conversation, each shared by the turns typing
/// there, so a second mention in a thread adds no second pulse. Cheap to
/// clone.
#[derive(Clone, Default)]
pub(crate) struct Pulses(Arc<Mutex<HashMap<String, Pulse>>>);

static NEXT: AtomicU64 = AtomicU64::new(0);

/// One turn's typing: whether it ended, and the pulse it joined.
#[derive(Default)]
struct Turn {
    ended: bool,
    joined: Option<(String, u64)>,
}

impl Pulses {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pulse>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Conversations with a pulse running, for tests.
    #[cfg(test)]
    pub(crate) fn running(&self) -> usize {
        self.lock().len()
    }
}

/// The typing event for a conversation: in the mention's thread, or in the
/// channel itself when `flat`.
pub(crate) fn template(event: &EventData, channel: &str, flat: bool) -> AppAgentEvent {
    let root = thread_root(event);
    let mut tags = vec![vec!["h".to_string(), channel.to_string()]];
    if !flat {
        if root != event.id {
            tags.push(vec!["e".into(), root, String::new(), "root".into()]);
        }
        tags.push(vec![
            "e".into(),
            event.id.clone(),
            String::new(),
            "reply".into(),
        ]);
    }
    AppAgentEvent {
        kind: 20002,
        content: String::new(),
        tags,
    }
}

/// Pulses typing, signed as the agent, where it answers `event` until the
/// returned end runs: in the channel itself when `top_level` settles true,
/// for a chat in the owner's Me, which reads as one conversation. It waits
/// for that answer, as its turn does, so both place it alike; a turn that
/// cannot tell where to answer fails, and shows no typing. Clients drop
/// typing when the agent replies, or a few seconds after its last pulse.
pub(crate) fn pulse_typing(
    publish: Publish,
    top_level: BoxFuture<'static, Result<bool, String>>,
    event: &EventData,
    channel: &str,
    pulses: Pulses,
    runtime: &tokio::runtime::Handle,
) -> End {
    // Changed under the pulses' lock, so joining and ending never cross.
    let state: Arc<Mutex<Turn>> = Arc::default();
    {
        let (state, pulses, event, channel) = (
            state.clone(),
            pulses.clone(),
            event.clone(),
            channel.to_string(),
        );
        let spawner = runtime.clone();
        runtime.spawn(async move {
            let Ok(flat) = top_level.await else {
                return;
            };
            let root = thread_root(&event);
            let key = format!("{channel}:{}", if flat { "" } else { root.as_str() });
            let mut running = pulses.lock();
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.ended {
                return;
            }
            let shared = running.entry(key.clone()).or_insert_with(|| {
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                let (stop, stopped) = oneshot::channel();
                spawner.spawn(pulse(
                    publish,
                    template(&event, &channel, flat),
                    stopped,
                    pulses.clone(),
                    key.clone(),
                    id,
                ));
                Pulse {
                    id,
                    turns: 0,
                    stop: Some(stop),
                }
            });
            shared.turns += 1;
            state.joined = Some((key, shared.id));
        });
    }
    Box::new(move || {
        let mut running = pulses.lock();
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.ended {
            return;
        }
        state.ended = true;
        let Some((key, id)) = state.joined.take() else {
            return;
        };
        if let Some(shared) = running.get_mut(&key).filter(|shared| shared.id == id) {
            shared.turns -= 1;
            if shared.turns == 0 {
                if let Some(stop) = shared.stop.take() {
                    let _ = stop.send(());
                }
                running.remove(&key);
            }
        }
    })
}

/// Sends `typing` every [`PULSE`] until stopped, for at most [`LONGEST`],
/// or until the community refuses [`FAILURES`] in a row. A slow community
/// gets the next pulse once this one is answered.
async fn pulse(
    publish: Publish,
    typing: AppAgentEvent,
    mut stopped: oneshot::Receiver<()>,
    pulses: Pulses,
    key: String,
    id: u64,
) {
    let started = tokio::time::Instant::now();
    let mut ticks = tokio::time::interval(PULSE);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut failed = 0;
    loop {
        tokio::select! {
            _ = &mut stopped => return,
            _ = ticks.tick() => {}
        }
        if started.elapsed() >= LONGEST {
            break;
        }
        match publish(typing.clone()).await {
            Ok(()) => failed = 0,
            // Typing must never break the run it decorates. A blip skips a
            // pulse; a community that keeps refusing them gets no more.
            Err(error) => {
                failed += 1;
                if failed >= FAILURES {
                    tracing::warn!(%error, "agent typing was not sent");
                    break;
                }
            }
        }
    }
    // A pulse that gave up is not joined; the next turn starts anew.
    let mut running = pulses.lock();
    if running.get(&key).is_some_and(|shared| shared.id == id) {
        running.remove(&key);
    }
}

#[cfg(test)]
mod tests;
