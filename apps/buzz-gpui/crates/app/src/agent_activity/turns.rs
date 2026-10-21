//! Turn states from observer evidence (`activity.ts` `fold`, `publish`):
//! a turn is working while fresh liveness keeps arriving on a listening
//! feed, ended once a terminal frame arrives, and unknown otherwise.
//! Typing from an agent that sends telemetry is extra working evidence.

use super::text;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// At most this many turns (and typing entries) are tracked.
pub const TURN_LIMIT: usize = 512;
/// A turn is working at most this long after its last evidence.
pub const FRESH_MS: u64 = 30_000;
/// A typing pulse is working evidence this long.
pub const TYPING_FRESH_MS: u64 = 8_000;
/// Evidence from up to this far in the future is believed.
const SKEW_MS: u64 = 5_000;

const STARTS: [&str; 5] = [
    "turn_started",
    "turn_liveness",
    "acp_read",
    "acp_write",
    "session_resolved",
];
const ENDS: [&str; 3] = ["turn_completed", "turn_error", "agent_panic"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TurnState {
    Working,
    Unknown,
    Ended,
}

/// A turn as views see it (`ActivityTurn`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityTurn {
    pub agent: String,
    pub turn_id: String,
    pub channel_id: Option<String>,
    /// The latest evidence, ms.
    pub timestamp: u64,
    pub state: TurnState,
    /// The messages that started it, with their threads: from retained
    /// signed messages its start named, never from typing. `None` when the
    /// start named none.
    pub requests: Option<Vec<TurnRequest>>,
}

/// A message a turn handles, and the thread it sits in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnRequest {
    pub message_id: String,
    pub thread_root_id: String,
}

/// What association needs to know of a retained message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggerMessage {
    pub id: String,
    pub kind: u16,
    /// Its `h` tags.
    pub channels: Vec<String>,
    /// It has `e` tags.
    pub references: bool,
    /// Its thread root, when it is a reply.
    pub root: Option<String>,
}

/// Working typing evidence (`Typing`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Typing {
    pub agent: String,
    pub channel_id: String,
    pub thread_root: Option<String>,
    /// The first sighting in this uninterrupted run, ms.
    pub started_at: u64,
    pub expires_at: u64,
}

struct Turn {
    agent: String,
    turn_id: String,
    channel_id: Option<String>,
    timestamp: u64,
    ended: bool,
    epoch: u64,
    triggering_event_ids: Option<Vec<String>>,
    requests: Vec<TurnRequest>,
    unresolved: Vec<String>,
}

impl Turn {
    /// Moves the triggers `find` now knows into requests (`associate`).
    fn associate(&mut self, find: &dyn Fn(&str) -> Option<TriggerMessage>) {
        let mut unresolved = Vec::new();
        for id in std::mem::take(&mut self.unresolved) {
            let Some(event) = find(&id) else {
                unresolved.push(id);
                continue;
            };
            if ![9, 40002, 40008].contains(&event.kind)
                || event.channels.len() != 1
                || Some(&event.channels[0]) != self.channel_id.as_ref()
                || (event.references && event.root.is_none())
            {
                continue;
            }
            self.requests.push(TurnRequest {
                thread_root_id: event.root.unwrap_or_else(|| event.id.clone()),
                message_id: id,
            });
        }
        self.unresolved = unresolved;
    }
}

/// A start's `triggeringEventIds`, when it is a list of at most 64 event
/// ids.
fn triggers(item: &Value) -> Option<Vec<String>> {
    let ids = item.get("payload")?.get("triggeringEventIds")?.as_array()?;
    if ids.len() > 64 {
        return None;
    }
    ids.iter()
        .map(|id| {
            id.as_str()
                .filter(|id| super::hex64(id))
                .map(str::to_string)
        })
        .collect()
}

#[derive(Default)]
pub struct Turns {
    turns: HashMap<String, Turn>,
    /// Forgotten turns were at most this old; older evidence cannot
    /// bring one back.
    evidence_floor: u64,
    /// Bumped whenever the feed stops listening: older evidence no longer
    /// proves current work.
    epoch: u64,
    typing: Vec<Typing>,
}

/// RFC 3339 in ms.
pub fn parse_time(value: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
}

impl Turns {
    pub fn reset(&mut self) {
        self.turns.clear();
        self.typing.clear();
        self.evidence_floor = 0;
        self.epoch += 1;
    }

    pub fn new_epoch(&mut self) {
        self.epoch += 1;
    }

    pub fn clear_typing(&mut self) {
        self.typing.clear();
    }

    /// Folds one telemetry item; returns how many turns were forgotten
    /// to stay within [`TURN_LIMIT`].
    pub fn fold(
        &mut self,
        agent: &str,
        item: &Value,
        now: u64,
        can_access: &dyn Fn(&str) -> bool,
        find: &dyn Fn(&str) -> Option<TriggerMessage>,
    ) -> usize {
        let Some(kind) = text(item.get("kind")) else {
            return 0;
        };
        let Some(turn_id) = text(item.get("turnId")) else {
            return 0;
        };
        let channel = match item.get("channelId") {
            Some(Value::Null) => None,
            Some(value) => match text(Some(value)) {
                Some(channel) => Some(channel),
                None => return 0,
            },
            None => return 0,
        };
        let Some(timestamp) = item.get("timestamp").and_then(Value::as_str) else {
            return 0;
        };
        if channel.is_some_and(|channel| !can_access(channel)) {
            return 0;
        }
        let ends = ENDS.contains(&kind);
        if !STARTS.contains(&kind) && !ends {
            return 0;
        }
        let Some(timestamp) = parse_time(timestamp).filter(|t| *t <= now + SKEW_MS) else {
            return 0;
        };
        let key = format!("{agent}:{turn_id}");
        let previous = self.turns.get(&key);
        // Terminal evidence wins, even over a delayed earlier heartbeat.
        if previous.is_some_and(|p| p.ended || (p.timestamp > timestamp && !ends)) {
            return 0;
        }
        if previous.is_none() && timestamp <= self.evidence_floor {
            return 0;
        }
        let highest = previous.map_or(timestamp, |p| p.timestamp.max(timestamp));
        let triggering_event_ids = match kind {
            "turn_started" => triggers(item),
            _ => previous.and_then(|p| p.triggering_event_ids.clone()),
        };
        let (requests, unresolved) = match (&triggering_event_ids, previous) {
            (None, _) => (Vec::new(), Vec::new()),
            (Some(_), Some(p)) if p.triggering_event_ids.is_some() => {
                (p.requests.clone(), p.unresolved.clone())
            }
            (Some(ids), _) => (Vec::new(), ids.clone()),
        };
        let mut turn = Turn {
            agent: agent.to_string(),
            turn_id: turn_id.to_string(),
            channel_id: channel.map(str::to_string),
            timestamp: highest,
            ended: ends,
            epoch: self.epoch,
            triggering_event_ids,
            requests,
            unresolved,
        };
        turn.associate(find);
        self.turns.insert(key, turn);
        if self.turns.len() > TURN_LIMIT
            && let Some((key, oldest)) = self
                .turns
                .iter()
                .min_by_key(|(_, turn)| turn.timestamp)
                .map(|(key, turn)| (key.clone(), turn.timestamp))
        {
            self.turns.remove(&key);
            self.evidence_floor = self.evidence_floor.max(oldest);
            return 1;
        }
        0
    }

    /// Every tracked turn with its state now.
    pub fn states(&self, now: u64, listening: bool) -> Vec<ActivityTurn> {
        let mut turns: Vec<ActivityTurn> = self
            .turns
            .values()
            .map(|turn| ActivityTurn {
                agent: turn.agent.clone(),
                turn_id: turn.turn_id.clone(),
                channel_id: turn.channel_id.clone(),
                timestamp: turn.timestamp,
                requests: turn
                    .triggering_event_ids
                    .as_ref()
                    .map(|_| turn.requests.clone()),
                state: if turn.ended {
                    TurnState::Ended
                } else if listening
                    && turn.epoch == self.epoch
                    && now.saturating_sub(turn.timestamp) <= FRESH_MS
                    && turn.timestamp <= now + SKEW_MS
                {
                    TurnState::Working
                } else {
                    TurnState::Unknown
                },
            })
            .collect();
        turns.sort_by(|a, b| {
            (a.timestamp, &a.agent, &a.turn_id).cmp(&(b.timestamp, &b.agent, &b.turn_id))
        });
        turns
    }

    /// Replaces typing evidence with who types now. A run keeps its start
    /// while it continues, and its expiry until half of it has passed, so
    /// asking again with nothing new leaves the snapshot as it was.
    pub fn typing(&mut self, entries: &[(String, String, Option<String>)], now: u64) {
        let mut next = Vec::new();
        for (agent, channel, root) in entries.iter().take(TURN_LIMIT) {
            let running = self
                .typing
                .iter()
                .find(|t| t.agent == *agent && t.channel_id == *channel && t.thread_root == *root)
                .filter(|t| now < t.expires_at);
            if let Some(typing) = running.filter(|t| t.expires_at - now > TYPING_FRESH_MS / 2) {
                next.push(typing.clone());
                continue;
            }
            let started_at = running.map_or(now, |t| t.started_at);
            next.push(Typing {
                agent: agent.clone(),
                channel_id: channel.clone(),
                thread_root: root.clone(),
                started_at,
                expires_at: now + TYPING_FRESH_MS,
            });
        }
        self.typing = next;
    }

    pub fn expire_typing(&mut self, now: u64, live_agents: &HashSet<String>) {
        self.typing
            .retain(|t| now < t.expires_at && live_agents.contains(&t.agent));
    }

    /// Trigger ids no retained message has answered yet.
    pub fn unresolved(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .turns
            .values()
            .flat_map(|turn| turn.unresolved.iter().cloned())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Associates triggers with messages that arrived after their
    /// telemetry (`messagesChanged`). True when any turn changed.
    pub fn resolve(&mut self, find: &dyn Fn(&str) -> Option<TriggerMessage>) -> bool {
        let mut changed = false;
        for turn in self.turns.values_mut() {
            if turn.unresolved.iter().any(|id| find(id).is_some()) {
                turn.associate(find);
                changed = true;
            }
        }
        changed
    }

    pub fn working_typing(&self) -> Vec<Typing> {
        self.typing.clone()
    }
}

/// Channels with working turns or channel-level typing, sorted
/// (`workingChannels`).
pub fn working_channels(turns: &[ActivityTurn], typing: &[Typing]) -> Vec<String> {
    let mut channels: Vec<String> = turns
        .iter()
        .filter(|turn| turn.state == TurnState::Working)
        .filter_map(|turn| turn.channel_id.clone())
        .chain(
            typing
                .iter()
                .filter(|t| t.thread_root.is_none())
                .map(|t| t.channel_id.clone()),
        )
        .collect();
    channels.sort();
    channels.dedup();
    channels
}
