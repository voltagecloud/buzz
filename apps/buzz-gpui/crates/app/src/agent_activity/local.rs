//! What this app's own agents are doing with each message they were asked
//! about (buzz-app's `local-activity.ts`): received, starting, working,
//! finished or failed, with the harness's latest step. App-owned facts
//! only: nothing is published to the relay, inferred or polled.
//!
//! [`LocalActivity`] is shared by the Agents runtime, which writes it, and
//! each community's activity feed, which shows its own scope's rows.

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui_kit::Global;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

/// How many rows are kept for display; settled ones go first.
const LIMIT: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LocalWorkState {
    Waiting,
    Working,
    Done,
    Error,
    Superseded,
}

impl LocalWorkState {
    fn settled(self) -> bool {
        matches!(self, Self::Done | Self::Error | Self::Superseded)
    }

    /// Whether the work is still to come or going on.
    pub fn active(self) -> bool {
        matches!(self, Self::Waiting | Self::Working)
    }

    /// Which row stands for an agent with several in one place.
    fn rank(self) -> u8 {
        match self {
            Self::Superseded | Self::Done => 0,
            Self::Error => 1,
            Self::Waiting => 2,
            Self::Working => 3,
        }
    }
}

/// How a request ended (`WorkOutcome`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkOutcome {
    pub ok: bool,
    pub error: Option<String>,
}

impl WorkOutcome {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
        }
    }
}

/// A harness step: its own tool name and, when known, what it acts on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkStep {
    pub tool: String,
    pub target: Option<String>,
}

/// One agent's handling of one message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalWork {
    /// The community and viewer it runs in ([`scope`]).
    pub scope: String,
    pub agent: String,
    pub channel_id: String,
    pub message_id: String,
    pub thread_root_id: String,
    pub state: LocalWorkState,
    /// Unix ms.
    pub started_at: u64,
    pub error: Option<String>,
    /// Latest live step, as the harness named it; display only.
    pub step: Option<WorkStep>,
    /// The owner hid this error here; the outcome remains.
    pub dismissed: bool,
}

/// The scope rows carry: a community's `https://` origin and its viewer.
pub fn scope(http_origin: &str, viewer: &str) -> String {
    format!("{http_origin}:{viewer}")
}

/// What [`LocalActivity::begin`] needs to know about a request.
#[derive(Clone, Debug)]
pub struct Begin {
    pub scope: String,
    pub agent: String,
    pub channel_id: String,
    pub message_id: String,
    pub thread_root_id: String,
}

#[derive(Default)]
struct Inner {
    entries: Vec<(u64, LocalWork)>,
    next: u64,
    work: Arc<[LocalWork]>,
    listeners: Vec<UnboundedSender<()>>,
}

impl Inner {
    fn publish(&mut self) {
        self.work = self.entries.iter().map(|(_, row)| row.clone()).collect();
        self.listeners
            .retain(|listener| listener.unbounded_send(()).is_ok());
    }
}

/// The shared store (`createLocalAgentActivity`). Cheap to clone.
#[derive(Clone, Default)]
pub struct LocalActivity(Arc<Mutex<Inner>>);

impl Global for LocalActivity {}

impl LocalActivity {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every row, oldest first.
    pub fn work(&self) -> Arc<[LocalWork]> {
        self.lock().work.clone()
    }

    /// Wakes once after each change.
    pub fn subscribe(&self) -> UnboundedReceiver<()> {
        let (sender, receiver) = unbounded();
        self.lock().listeners.push(sender);
        receiver
    }

    /// Starts showing a request as received. The newest handling of one
    /// message replaces an older one; the older handle can no longer
    /// change it.
    pub fn begin(&self, input: Begin, now: u64) -> WorkHandle {
        let mut inner = self.lock();
        inner.entries.retain(|(_, row)| {
            !(row.scope == input.scope
                && row.agent == input.agent
                && row.message_id == input.message_id)
        });
        if inner.entries.len() >= LIMIT {
            let victim = inner
                .entries
                .iter()
                .position(|(_, row)| row.state.settled())
                .unwrap_or(0);
            inner.entries.remove(victim);
        }
        let id = inner.next;
        inner.next += 1;
        inner.entries.push((
            id,
            LocalWork {
                scope: input.scope,
                agent: input.agent,
                channel_id: input.channel_id,
                message_id: input.message_id,
                thread_root_id: input.thread_root_id,
                state: LocalWorkState::Waiting,
                started_at: now,
                error: None,
                step: None,
                dismissed: false,
            },
        ));
        inner.publish();
        WorkHandle {
            store: self.clone(),
            id,
        }
    }

    /// Forgets every row of `scope` and/or `agent` (`None` matches all).
    pub fn clear(&self, scope: Option<&str>, agent: Option<&str>) {
        let mut inner = self.lock();
        let before = inner.entries.len();
        inner.entries.retain(|(_, row)| {
            !(scope.is_none_or(|scope| row.scope == scope)
                && agent.is_none_or(|agent| row.agent == agent))
        });
        if inner.entries.len() != before {
            inner.publish();
        }
    }

    /// Hides one failure on this device; its outcome stays.
    pub fn dismiss_error(&self, scope: &str, agent: &str, message_id: &str) {
        let mut inner = self.lock();
        let found = inner.entries.iter_mut().find(|(_, row)| {
            row.state == LocalWorkState::Error
                && !row.dismissed
                && row.scope == scope
                && row.agent == agent
                && row.message_id == message_id
        });
        if let Some((_, row)) = found {
            row.dismissed = true;
            inner.publish();
        }
    }

    fn update(
        &self,
        id: u64,
        state: LocalWorkState,
        error: Option<String>,
        step: Option<WorkStep>,
    ) {
        let mut inner = self.lock();
        let Some((_, current)) = inner.entries.iter_mut().find(|(row, _)| *row == id) else {
            return;
        };
        if current.state.settled() {
            return;
        }
        let next = step.or_else(|| {
            (state == LocalWorkState::Working)
                .then(|| current.step.clone())
                .flatten()
        });
        let error = error.map(|error| error.chars().take(500).collect::<String>());
        if current.state == state && current.error == error && current.step == next {
            return;
        }
        current.state = state;
        current.error = error;
        current.step = next;
        inner.publish();
    }

    fn remove(&self, id: u64) {
        let mut inner = self.lock();
        let before = inner.entries.len();
        inner.entries.retain(|(row, _)| *row != id);
        if inner.entries.len() != before {
            inner.publish();
        }
    }
}

/// One request's row, for the runtime and the harness to move along.
#[derive(Clone)]
pub struct WorkHandle {
    store: LocalActivity,
    id: u64,
}

impl WorkHandle {
    /// The harness took the request (not proof the model read it).
    pub fn started(&self) {
        self.store
            .update(self.id, LocalWorkState::Working, None, None);
    }

    /// A newer request the harness acknowledged carries on this one's
    /// work: hide this bubble without claiming it succeeded.
    pub fn supersede(&self) {
        self.store
            .update(self.id, LocalWorkState::Superseded, None, None);
    }

    /// The harness's latest step; display only.
    pub fn step(&self, tool: &str, target: Option<&str>) {
        let tool: String = tool.trim().chars().take(80).collect();
        if tool.is_empty() {
            return;
        }
        let target = target
            .map(|target| {
                target
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(160)
                    .collect::<String>()
            })
            .filter(|target| !target.is_empty());
        self.store.update(
            self.id,
            LocalWorkState::Working,
            None,
            Some(WorkStep { tool, target }),
        );
    }

    pub fn finish(&self, outcome: WorkOutcome) {
        if outcome.ok {
            self.store.update(self.id, LocalWorkState::Done, None, None);
        } else {
            let error = outcome
                .error
                .filter(|error| !error.is_empty())
                .unwrap_or_else(|| "Could not finish this request".into());
            self.store
                .update(self.id, LocalWorkState::Error, Some(error), None);
        }
    }

    /// The type declined the request: there is nothing to show.
    pub fn remove(&self) {
        self.store.remove(self.id);
    }
}

/// Received, or handed over but not yet on a first step: shown as
/// breathing "Starting".
pub fn breathing(row: Option<&LocalWork>) -> bool {
    row.is_some_and(|row| {
        row.state == LocalWorkState::Waiting
            || (row.state == LocalWorkState::Working && row.step.is_none())
    })
}

/// Each agent's most telling row in `channel` (or one thread of it):
/// working over waiting over failed over settled (`localActivity`).
pub fn local_activity<'a>(
    local: &'a [LocalWork],
    channel: &str,
    thread_root: Option<&str>,
) -> BTreeMap<&'a str, &'a LocalWork> {
    let mut agents: BTreeMap<&str, &LocalWork> = BTreeMap::new();
    for row in local.iter().filter(|row| {
        row.channel_id == channel && thread_root.is_none_or(|root| row.thread_root_id == root)
    }) {
        match agents.get(row.agent.as_str()) {
            Some(previous) if row.state.rank() <= previous.state.rank() => {}
            _ => {
                agents.insert(&row.agent, row);
            }
        }
    }
    agents
}

/// The row's line: "Received · waiting to start", "Starting", the step,
/// its failure or "Finished" (`localWorkLabel`).
pub fn local_work_label(
    state: LocalWorkState,
    error: Option<&str>,
    step: Option<&WorkStep>,
) -> String {
    match state {
        LocalWorkState::Superseded => "Continued on a later message".into(),
        LocalWorkState::Waiting => "Received · waiting to start".into(),
        LocalWorkState::Working => step.map_or_else(|| "Starting".into(), step_text),
        LocalWorkState::Error => error
            .filter(|error| !error.is_empty())
            .unwrap_or("Could not finish this request")
            .into(),
        LocalWorkState::Done => "Finished".into(),
    }
}

/// Fallback phrases for steps without a target.
fn verb(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "Bash" | "Shell" => "Running a command",
        "Read" => "Reading a file",
        "Write" => "Writing a file",
        "Edit" | "MultiEdit" => "Editing a file",
        "NotebookEdit" => "Editing a notebook",
        "Grep" => "Searching files",
        "Glob" => "Finding files",
        "WebFetch" => "Reading a web page",
        "WebSearch" => "Searching the web",
        "Task" | "Agent" => "Running a subtask",
        "TodoWrite" => "Updating its plan",
        "Thinking" => "Thinking",
        "Writing reply" => "Writing a reply",
        _ => return None,
    })
}

/// Readable "doing what" with a target; Claude's command descriptions are
/// already plain English.
fn readable(tool: &str, target: &str) -> Option<String> {
    Some(match tool {
        "Bash" => target.to_string(),
        "Shell" => format!("Running {target}"),
        "Read" => format!("Reading {target}"),
        "Write" => format!("Writing {target}"),
        "Edit" | "MultiEdit" | "NotebookEdit" => format!("Editing {target}"),
        "Grep" => format!("Searching for {target}"),
        "ListFiles" => format!("Listing files in {target}"),
        "ImageView" => format!("Viewing {target}"),
        "Glob" => format!("Finding {target}"),
        "WebFetch" => format!("Reading {target}"),
        "WebSearch" => format!("Searching the web for {target}"),
        "Task" | "Agent" => format!("Subagent · {target}"),
        _ => return None,
    })
}

fn untargeted(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "send" | "send_message" => "Sending a message",
        "read" => "Reading the conversation",
        "TodoWrite" => "Updating its plan",
        "ListFiles" => "Listing files",
        "ImageView" => "Viewing an image",
        "Compacting" => "Compacting its context",
        _ => return None,
    })
}

/// A step in words, from a fixed lookup over the harness's own names
/// (`stepText`); no model rewrites harness text.
pub fn step_text(step: &WorkStep) -> String {
    if let Some(target) = &step.target
        && let Some(phrase) = readable(&step.tool, target)
    {
        return phrase;
    }
    if let Some(known) = untargeted(&step.tool).or_else(|| verb(&step.tool)) {
        return known.into();
    }
    match &step.target {
        Some(target) => format!("{} · {target}", step.tool),
        None => step.tool.clone(),
    }
}

#[cfg(test)]
mod tests;
