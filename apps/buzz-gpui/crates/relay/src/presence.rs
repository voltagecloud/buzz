//! Presence (`presence.ts`, spec §5h.1–5h.5): who in this community is
//! online, away or offline, and our own status.
//!
//! [`PresenceDirectory`] holds volatile evidence for at most 256 demanded
//! people. Evidence comes from relay-signed HTTP snapshots, from live kind
//! 20001 events and from our own accepted publications, and expires after
//! [`EVIDENCE_MS`]. Expired or missing evidence is *unknown*: no badge, never
//! "offline".
//!
//! [`PresencePublisher`] decides when to publish our own status over the
//! authenticated socket: renewed every 60–65 s, at once on a change, never
//! through the outbox and never "offline" on close.
//!
//! Both are synchronous and take the wall clock in milliseconds; the session
//! runs the reads and publications they ask for.

use crate::event::{Verified, Verifier, is_hex64, kinds};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

/// Most people whose presence is followed (`presence.ts`).
pub const MAX_SUBJECTS: usize = 256;
/// Evidence becomes unknown this long after it was observed.
pub const EVIDENCE_MS: u64 = 75_000;
/// New demand coalesces this long before a read.
const COALESCE_MS: u64 = 100;
/// A read starts at least this long after the previous one settled.
const READ_GATE_MS: u64 = 5_000;
/// At most one publication per this interval (`live.ts` send gate).
const SEND_GATE_MS: u64 = 5_000;

/// A status on the wire. Unknown is the absence of evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PresenceState {
    Online,
    Away,
    Offline,
}

impl PresenceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Away => "away",
            Self::Offline => "offline",
        }
    }

    /// Bare status content, or the legacy `{"status":"…"}` form. Other
    /// strings are unknown.
    pub fn parse(content: &str) -> Option<Self> {
        let status = if content.starts_with('{') {
            let value: Value = serde_json::from_str(content).ok()?;
            value.get("status")?.as_str()?.to_owned()
        } else {
            content.to_owned()
        };
        match status.as_str() {
            "online" => Some(Self::Online),
            "away" => Some(Self::Away),
            "offline" => Some(Self::Offline),
            _ => None,
        }
    }
}

/// The device-local status choice (`buzz-presence.v1`). `Auto` publishes
/// online, or away while the machine is idle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PresenceMode {
    #[default]
    Auto,
    Away,
    Offline,
}

/// A uniformly random number below `bound` (jitter only).
pub(crate) fn random_below(bound: u64) -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(bound);
    hasher.finish() % bound.max(1)
}

/// The renewal and refresh interval: 60–65 s.
fn minute() -> u64 {
    60_000 + random_below(5_000)
}

/// A snapshot read the directory wants run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRead {
    pub authors: Vec<String>,
    generation: u64,
    started: u64,
    /// Each author's evidence revision when the read started; newer live
    /// evidence wins over the read's result.
    revisions: Vec<u64>,
}

impl SnapshotRead {
    /// The `POST /query` body: exactly one presence filter.
    pub fn body(&self) -> String {
        serde_json::json!([{
            "kinds": [kinds::PRESENCE],
            "authors": self.authors,
            "limit": self.authors.len(),
        }])
        .to_string()
    }
}

/// How a snapshot read ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotResult {
    /// Each requested author's status; `None` for unsupported strings.
    Statuses(HashMap<String, Option<PresenceState>>),
    /// Network failure, invalid response or untrusted snapshot. `retry_ms`
    /// is the relay's cooldown, if it gave one.
    Failed { retry_ms: Option<u64> },
}

/// Validates a relay-signed snapshot (`parsePresence`). Any bad event
/// rejects the whole snapshot; requested authors absent from a valid one
/// are offline.
pub fn parse_snapshot(
    raw: &Value,
    authors: &[String],
    relay: &str,
    verifier: &Verifier,
) -> Result<HashMap<String, Option<PresenceState>>, String> {
    let items = raw
        .as_array()
        .filter(|items| items.len() <= authors.len())
        .ok_or("Invalid presence snapshot")?;
    let requested: HashSet<&str> = authors.iter().map(String::as_str).collect();
    let mut values = HashMap::new();
    for item in items {
        let event = verifier
            .verify_json(item)
            .map_err(|_| "Untrusted presence snapshot")?;
        let subjects: Vec<&[String]> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .filter(|tag| tag.first().map(String::as_str) == Some("p"))
            .collect();
        let subject = match subjects.as_slice() {
            [tag] if tag.len() == 2 => tag[1].as_str(),
            _ => return Err("Untrusted presence snapshot".into()),
        };
        if event.kind_u16() != kinds::PRESENCE
            || event.pubkey_hex() != relay
            || !requested.contains(subject)
            || values.contains_key(subject)
        {
            return Err("Untrusted presence snapshot".into());
        }
        values.insert(subject.to_owned(), PresenceState::parse(&event.content));
    }
    for author in authors {
        values
            .entry(author.clone())
            .or_insert(Some(PresenceState::Offline));
    }
    Ok(values)
}

struct Evidence {
    state: PresenceState,
    expires: u64,
}

/// Observed presence for the demanded people.
pub struct PresenceDirectory {
    /// Followed people, at most [`MAX_SUBJECTS`], the viewer first.
    demand: Vec<String>,
    viewer: String,
    evidence: HashMap<String, Evidence>,
    revisions: HashMap<String, u64>,
    /// Observing: the socket is connected and the window is shown.
    connected: bool,
    socket: bool,
    /// The app's window is on screen (`activity.visible()`); while it is
    /// hidden nothing is observed and everyone is unknown.
    visible: bool,
    /// Bumped on disconnect; fences reads started before.
    generation: u64,
    reading: bool,
    next_read: Option<u64>,
    gate: u64,
    snapshot: Arc<HashMap<String, PresenceState>>,
    dirty: bool,
}

impl PresenceDirectory {
    pub fn new(viewer: impl Into<String>) -> Self {
        Self {
            demand: Vec::new(),
            viewer: viewer.into(),
            evidence: HashMap::new(),
            revisions: HashMap::new(),
            connected: false,
            socket: false,
            visible: true,
            generation: 0,
            reading: false,
            next_read: None,
            gate: 0,
            snapshot: Arc::default(),
            dirty: false,
        }
    }

    pub fn demand(&self) -> &[String] {
        &self.demand
    }

    /// Replaces who is followed: the viewer first, then in the given
    /// order, up to [`MAX_SUBJECTS`]. Dropped people lose their evidence;
    /// new ones are read after a short coalescing delay.
    pub fn set_demand(&mut self, authors: impl IntoIterator<Item = String>, now: u64) {
        let mut seen = HashSet::new();
        let mut next: Vec<String> = authors
            .into_iter()
            .filter(|author| is_hex64(author) && seen.insert(author.clone()))
            .collect();
        if let Some(position) = next.iter().position(|author| *author == self.viewer) {
            let viewer = next.remove(position);
            next.insert(0, viewer);
        }
        next.truncate(MAX_SUBJECTS);
        if next == self.demand {
            return;
        }
        let wanted: HashSet<&String> = next.iter().collect();
        let before = self.evidence.len();
        self.evidence.retain(|author, _| wanted.contains(author));
        self.revisions.retain(|author, _| wanted.contains(author));
        self.dirty |= self.evidence.len() != before;
        let previous: HashSet<&String> = self.demand.iter().collect();
        let added = next.iter().any(|author| !previous.contains(author));
        if next.is_empty() {
            self.next_read = None;
        } else if added {
            let initial = (now + COALESCE_MS).max(self.gate);
            self.next_read = Some(match self.next_read {
                Some(at) if !self.demand.is_empty() => at.min(initial),
                _ => initial,
            });
        }
        self.demand = next;
    }

    /// Reads everyone again at the next allowed moment (`refresh`): a
    /// harness publishes presence just after a start or stop, so a read
    /// can race it.
    pub fn refresh(&mut self, now: u64) {
        if !self.connected || self.demand.is_empty() {
            return;
        }
        let at = now.max(self.gate);
        self.next_read = Some(self.next_read.map_or(at, |next| next.min(at)));
    }

    /// The live socket's state. Either way all evidence becomes unknown:
    /// nothing tells us what changed while it was down.
    pub fn set_connected(&mut self, connected: bool, now: u64) {
        self.socket = connected;
        self.observe(now);
    }

    /// The window was shown or hidden: hidden stops observation and clears
    /// what was observed; shown reads again soon.
    pub fn set_visible(&mut self, visible: bool, now: u64) {
        self.visible = visible;
        self.observe(now);
    }

    fn observe(&mut self, now: u64) {
        let connected = self.socket && self.visible;
        if connected == self.connected {
            return;
        }
        self.connected = connected;
        self.generation += 1;
        self.reading = false;
        self.dirty |= !self.evidence.is_empty();
        self.evidence.clear();
        self.next_read = connected.then(|| (now + COALESCE_MS).max(self.gate));
    }

    /// The next wall-clock time [`Self::tick`] or [`Self::due`] has work.
    pub fn deadline(&self) -> Option<u64> {
        let read = self
            .next_read
            .filter(|_| self.connected && !self.reading && !self.demand.is_empty());
        let expiry = self
            .evidence
            .values()
            .map(|evidence| evidence.expires)
            .min();
        [read, expiry].into_iter().flatten().min()
    }

    /// Expires old evidence.
    pub fn tick(&mut self, now: u64) {
        let before = self.evidence.len();
        self.evidence.retain(|_, evidence| evidence.expires > now);
        self.dirty |= self.evidence.len() != before;
    }

    /// Starts a snapshot read when one is due.
    pub fn due(&mut self, now: u64) -> Option<SnapshotRead> {
        self.tick(now);
        if !self.connected
            || self.reading
            || self.demand.is_empty()
            || self.next_read.is_none_or(|at| at > now)
        {
            return None;
        }
        self.reading = true;
        self.next_read = None;
        self.gate = now + READ_GATE_MS;
        Some(SnapshotRead {
            authors: self.demand.clone(),
            generation: self.generation,
            started: now,
            revisions: self
                .demand
                .iter()
                .map(|author| self.revisions.get(author).copied().unwrap_or(0))
                .collect(),
        })
    }

    /// Applies a finished read. Authors with newer live evidence, or no
    /// longer followed, keep what they have.
    pub fn finish(&mut self, read: SnapshotRead, result: SnapshotResult, now: u64) {
        if read.generation != self.generation {
            return;
        }
        self.reading = false;
        let current = |this: &Self, author: &String, revision: u64| {
            this.demand.contains(author)
                && this.revisions.get(author).copied().unwrap_or(0) == revision
        };
        match result {
            SnapshotResult::Statuses(values) => {
                for (author, revision) in read.authors.iter().zip(&read.revisions) {
                    if !current(self, author, *revision) {
                        continue;
                    }
                    let expires = read.started + EVIDENCE_MS;
                    let state = values
                        .get(author)
                        .copied()
                        .flatten()
                        .filter(|_| expires > now);
                    self.set(author, state, expires);
                }
                // Demand added while reading may already want a sooner read.
                let refresh = now + minute();
                self.next_read = Some(self.next_read.map_or(refresh, |at| at.min(refresh)));
            }
            SnapshotResult::Failed { retry_ms } => {
                for (author, revision) in read.authors.iter().zip(&read.revisions) {
                    if current(self, author, *revision) {
                        self.set(author, None, 0);
                    }
                }
                self.next_read = Some(now + minute().max(retry_ms.unwrap_or(0)));
            }
        }
        self.gate = self.gate.max(now + READ_GATE_MS);
        self.next_read = self.next_read.map(|at| at.max(self.gate));
    }

    /// A live kind 20001 event from a followed author.
    pub fn live(&mut self, event: &Verified, now: u64) {
        let author = event.pubkey_hex();
        if !self.connected || event.kind_u16() != kinds::PRESENCE || !self.demand.contains(&author)
        {
            return;
        }
        *self.revisions.entry(author.clone()).or_default() += 1;
        self.set(
            &author,
            PresenceState::parse(&event.content),
            now + EVIDENCE_MS,
        );
    }

    /// The relay accepted our own publication.
    pub fn accepted(&mut self, state: PresenceState, now: u64) {
        let viewer = self.viewer.clone();
        if !self.connected || !self.demand.contains(&viewer) {
            return;
        }
        *self.revisions.entry(viewer.clone()).or_default() += 1;
        self.set(&viewer, Some(state), now + EVIDENCE_MS);
    }

    fn set(&mut self, author: &str, state: Option<PresenceState>, expires: u64) {
        match state {
            Some(state) => {
                let changed = self
                    .evidence
                    .get(author)
                    .is_none_or(|evidence| evidence.state != state);
                self.evidence
                    .insert(author.to_owned(), Evidence { state, expires });
                self.dirty |= changed;
            }
            None => self.dirty |= self.evidence.remove(author).is_some(),
        }
    }

    /// Known statuses; absent people are unknown. The same `Arc` until
    /// something changes.
    pub fn snapshot(&mut self) -> Arc<HashMap<String, PresenceState>> {
        if std::mem::take(&mut self.dirty) {
            let next: HashMap<String, PresenceState> = self
                .evidence
                .iter()
                .map(|(author, evidence)| (author.clone(), evidence.state))
                .collect();
            if *self.snapshot != next {
                self.snapshot = Arc::new(next);
            }
        }
        self.snapshot.clone()
    }
}

/// A publication the publisher wants sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Publication {
    pub state: PresenceState,
    generation: u64,
}

/// How a publication ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    Accepted,
    /// Not sent: the socket was not connected.
    NotSent,
    /// Not sent: the relay's quota cooldown runs this many more ms.
    Cooling(u64),
    /// Refused (a quota refusal pauses the shared cooldown at the
    /// socket), no answer in time, or the connection dropped first.
    Unconfirmed,
}

/// When to publish our own status.
pub struct PresencePublisher {
    mode: PresenceMode,
    idle: bool,
    connected: bool,
    /// Bumped whenever intent or the connection changes; a result for an
    /// older generation is dropped.
    generation: u64,
    in_flight: bool,
    next: Option<u64>,
    gate: u64,
    /// An accepted offline is a clear, not a lease: no renewal.
    stopped: bool,
}

impl Default for PresencePublisher {
    fn default() -> Self {
        Self {
            mode: PresenceMode::Auto,
            idle: false,
            connected: false,
            generation: 0,
            in_flight: false,
            next: None,
            gate: 0,
            stopped: false,
        }
    }
}

impl PresencePublisher {
    pub fn desired(&self) -> PresenceState {
        match self.mode {
            PresenceMode::Auto if self.idle => PresenceState::Away,
            PresenceMode::Auto => PresenceState::Online,
            PresenceMode::Away => PresenceState::Away,
            PresenceMode::Offline => PresenceState::Offline,
        }
    }

    pub fn set_mode(&mut self, mode: PresenceMode, now: u64) {
        let before = self.desired();
        self.mode = mode;
        self.changed(before, now);
    }

    /// The machine has been idle long enough to show away (auto mode only).
    pub fn set_idle(&mut self, idle: bool, now: u64) {
        let before = self.desired();
        self.idle = idle;
        self.changed(before, now);
    }

    /// A new intent replaces any in-flight one and goes out at once (still
    /// behind the send gate).
    fn changed(&mut self, before: PresenceState, now: u64) {
        if self.desired() == before {
            return;
        }
        self.restart();
        if self.connected {
            self.next = Some(now);
        }
    }

    fn restart(&mut self) {
        self.generation += 1;
        self.in_flight = false;
        self.stopped = false;
    }

    /// The first publication after connecting waits 250–1000 ms.
    pub fn set_connected(&mut self, connected: bool, now: u64) {
        if connected == self.connected {
            return;
        }
        self.connected = connected;
        self.restart();
        self.next = connected.then(|| now + 250 + random_below(750));
    }

    pub fn deadline(&self) -> Option<u64> {
        if !self.connected || self.in_flight || self.stopped {
            return None;
        }
        self.next.map(|at| at.max(self.gate))
    }

    /// Starts a publication when one is due.
    pub fn due(&mut self, now: u64) -> Option<Publication> {
        if self.deadline().is_none_or(|at| at > now) {
            return None;
        }
        self.in_flight = true;
        self.next = None;
        self.gate = now + SEND_GATE_MS;
        Some(Publication {
            state: self.desired(),
            generation: self.generation,
        })
    }

    /// Schedules what follows a publication. Returns the status the relay
    /// accepted, for our own presence evidence.
    pub fn finished(
        &mut self,
        publication: Publication,
        outcome: PublishOutcome,
        now: u64,
    ) -> Option<PresenceState> {
        if publication.generation != self.generation {
            return None;
        }
        self.in_flight = false;
        let delay = match &outcome {
            PublishOutcome::Accepted if publication.state == PresenceState::Offline => {
                self.stopped = true;
                self.next = None;
                return Some(publication.state);
            }
            PublishOutcome::Accepted => minute(),
            PublishOutcome::NotSent => 5_000 + random_below(1_000),
            PublishOutcome::Cooling(ms) => ms + 50,
            PublishOutcome::Unconfirmed => minute(),
        };
        self.next = Some(now + delay);
        (outcome == PublishOutcome::Accepted).then_some(publication.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_identity::{Identity, Purpose};
    use serde_json::json;

    fn hex(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn presence_event(identity: &Identity, content: &str, tags: Vec<Vec<String>>) -> Value {
        let unsigned = identity
            .template(
                Purpose::Presence,
                kinds::PRESENCE,
                1_760_000_000,
                tags,
                content.into(),
            )
            .unwrap();
        serde_json::to_value(identity.sign(Purpose::Presence, unsigned).unwrap()).unwrap()
    }

    fn live(identity: &Identity, content: &str) -> Verified {
        Verifier::default()
            .verify_json(&presence_event(identity, content, vec![]))
            .unwrap()
    }

    fn connected(viewer: &str, demand: &[String]) -> PresenceDirectory {
        let mut directory = PresenceDirectory::new(viewer);
        directory.set_connected(true, 0);
        directory.set_demand(demand.iter().cloned(), 0);
        directory
    }

    #[test]
    fn parses_bare_and_legacy_statuses() {
        assert_eq!(PresenceState::parse("online"), Some(PresenceState::Online));
        assert_eq!(
            PresenceState::parse(r#"{"status":"away"}"#),
            Some(PresenceState::Away)
        );
        assert_eq!(PresenceState::parse("busy"), None);
        assert_eq!(PresenceState::parse("{oops"), None);
    }

    #[test]
    fn demand_puts_the_viewer_first_and_caps_at_256() {
        let viewer = hex(0);
        let mut directory = PresenceDirectory::new(viewer.clone());
        let mut authors: Vec<String> = (0..300u32)
            .map(|n| format!("{n:064x}"))
            .filter(|author| *author != viewer)
            .collect();
        authors.push(viewer.clone());
        authors.push("not hex".into());
        directory.set_demand(authors, 0);
        assert_eq!(directory.demand().len(), MAX_SUBJECTS);
        assert_eq!(directory.demand()[0], viewer);
    }

    #[test]
    fn refresh_reads_again_at_once() {
        let a = hex(1);
        let mut directory = connected(&hex(0), std::slice::from_ref(&a));
        directory.refresh(10);
        assert_eq!(directory.deadline(), Some(10));
        let mut idle = PresenceDirectory::new(hex(0));
        idle.refresh(10);
        assert_eq!(idle.deadline(), None);
    }

    #[test]
    fn new_demand_coalesces_and_reads_respect_the_gate() {
        let (a, b) = (hex(1), hex(2));
        let mut directory = connected(&hex(0), std::slice::from_ref(&a));
        assert_eq!(directory.deadline(), Some(COALESCE_MS));
        assert!(directory.due(50).is_none());
        let read = directory.due(100).unwrap();
        assert_eq!(read.authors, vec![a.clone()]);
        assert_eq!(
            serde_json::from_str::<Value>(&read.body()).unwrap(),
            json!([{"kinds": [20001], "authors": [a], "limit": 1}])
        );
        // Nothing else starts while one runs.
        directory.set_demand([a.clone(), b.clone()], 200);
        assert!(directory.due(300).is_none());
        let statuses = HashMap::from([(a.clone(), Some(PresenceState::Online))]);
        directory.finish(read, SnapshotResult::Statuses(statuses), 1_000);
        assert_eq!(directory.snapshot().get(&a), Some(&PresenceState::Online));
        // The new author is read soon, but not before the five-second gate.
        assert_eq!(directory.deadline(), Some(6_000));
        let read = directory.due(6_000).unwrap();
        assert_eq!(read.authors.len(), 2);
    }

    #[test]
    fn evidence_expires_to_unknown() {
        let a = hex(1);
        let mut directory = connected(&hex(0), std::slice::from_ref(&a));
        let read = directory.due(100).unwrap();
        directory.finish(
            read,
            SnapshotResult::Statuses(HashMap::from([(a.clone(), Some(PresenceState::Away))])),
            200,
        );
        let before = directory.snapshot();
        assert_eq!(before.get(&a), Some(&PresenceState::Away));
        // Unchanged evidence keeps the same snapshot.
        assert!(Arc::ptr_eq(&before, &directory.snapshot()));
        // The refresh (60–65 s) comes before the 75 s expiry.
        assert!(directory.deadline().unwrap() >= 60_200);
        directory.tick(100 + EVIDENCE_MS);
        assert!(directory.snapshot().is_empty());
    }

    #[test]
    fn live_evidence_beats_an_older_snapshot() {
        let alice = Identity::ephemeral();
        let a = alice.pubkey_hex();
        let mut directory = connected(&hex(0), std::slice::from_ref(&a));
        let read = directory.due(100).unwrap();
        directory.live(&live(&alice, "online"), 150);
        directory.finish(
            read,
            SnapshotResult::Statuses(HashMap::from([(a.clone(), Some(PresenceState::Offline))])),
            200,
        );
        assert_eq!(directory.snapshot().get(&a), Some(&PresenceState::Online));
        // Undemanded authors are ignored.
        let bob = Identity::ephemeral();
        directory.live(&live(&bob, "online"), 300);
        assert!(!directory.snapshot().contains_key(&bob.pubkey_hex()));
    }

    #[test]
    fn failures_make_everyone_unknown_and_back_off() {
        let a = hex(1);
        let mut directory = connected(&hex(0), std::slice::from_ref(&a));
        let read = directory.due(100).unwrap();
        directory.finish(
            read,
            SnapshotResult::Statuses(HashMap::from([(a.clone(), Some(PresenceState::Online))])),
            200,
        );
        let read = directory.due(70_000).unwrap();
        directory.finish(
            read,
            SnapshotResult::Failed {
                retry_ms: Some(120_000),
            },
            70_100,
        );
        assert!(directory.snapshot().is_empty());
        assert_eq!(directory.deadline(), Some(190_100));
    }

    #[test]
    fn a_hidden_window_observes_nobody() {
        let a = hex(1);
        let mut directory = connected(&hex(0), std::slice::from_ref(&a));
        let read = directory.due(100).unwrap();
        directory.finish(
            read,
            SnapshotResult::Statuses(HashMap::from([(a.clone(), Some(PresenceState::Online))])),
            200,
        );
        assert!(!directory.snapshot().is_empty());
        directory.set_visible(false, 300);
        assert!(directory.snapshot().is_empty());
        assert_eq!(directory.due(400_000), None);
        // Reconnecting while hidden still reads nothing.
        directory.set_connected(false, 500);
        directory.set_connected(true, 600);
        assert_eq!(directory.due(400_000), None);
        directory.set_visible(true, 700);
        assert!(directory.due(400_000).is_some());
    }

    #[test]
    fn disconnect_clears_and_fences_reads() {
        let a = hex(1);
        let mut directory = connected(&hex(0), std::slice::from_ref(&a));
        let read = directory.due(100).unwrap();
        directory.set_connected(false, 150);
        directory.set_connected(true, 160);
        directory.finish(
            read,
            SnapshotResult::Statuses(HashMap::from([(a.clone(), Some(PresenceState::Online))])),
            200,
        );
        assert!(directory.snapshot().is_empty());
        // The reconnect read still waits for the gate of the fenced one.
        assert!(directory.due(5_099).is_none());
        assert!(directory.due(5_100).is_some());
    }

    #[test]
    fn removed_people_lose_evidence() {
        let (a, b) = (hex(1), hex(2));
        let mut directory = connected(&hex(0), &[a.clone(), b.clone()]);
        let read = directory.due(100).unwrap();
        directory.finish(
            read,
            SnapshotResult::Statuses(HashMap::from([
                (a.clone(), Some(PresenceState::Online)),
                (b.clone(), Some(PresenceState::Away)),
            ])),
            200,
        );
        directory.set_demand([b.clone()], 300);
        let snapshot = directory.snapshot();
        assert!(!snapshot.contains_key(&a));
        assert_eq!(snapshot.get(&b), Some(&PresenceState::Away));
        // Shrinking demand reads nothing new.
        assert!(directory.deadline().unwrap() > 60_000);
        directory.set_demand(Vec::new(), 400);
        assert!(directory.snapshot().is_empty());
        assert_eq!(directory.deadline(), None);
    }

    #[test]
    fn snapshots_must_be_signed_by_the_relay_for_requested_people() {
        let relay = Identity::ephemeral();
        let verifier = Verifier::default();
        let (a, b, c) = (hex(1), hex(2), hex(3));
        let authors = vec![a.clone(), b.clone(), c.clone()];
        let p = |subject: &str| vec![vec!["p".to_string(), subject.to_string()]];
        let raw = json!([
            presence_event(&relay, "online", p(&a)),
            presence_event(&relay, r#"{"status":"away"}"#, p(&b)),
        ]);
        let values = parse_snapshot(&raw, &authors, &relay.pubkey_hex(), &verifier).unwrap();
        assert_eq!(values[&a], Some(PresenceState::Online));
        assert_eq!(values[&b], Some(PresenceState::Away));
        assert_eq!(values[&c], Some(PresenceState::Offline));

        // Unsupported strings are unknown for that subject only.
        let raw = json!([presence_event(&relay, "dnd", p(&a))]);
        let values = parse_snapshot(&raw, &authors, &relay.pubkey_hex(), &verifier).unwrap();
        assert_eq!(values[&a], None);
        assert_eq!(values[&b], Some(PresenceState::Offline));

        let stranger = Identity::ephemeral();
        for raw in [
            json!([presence_event(&stranger, "online", p(&a))]),
            json!([presence_event(&relay, "online", p(&hex(9)))]),
            json!([
                presence_event(&relay, "online", p(&a)),
                presence_event(&relay, "away", p(&a))
            ]),
            json!([presence_event(&relay, "online", vec![])]),
            json!({"not": "an array"}),
        ] {
            assert!(parse_snapshot(&raw, &authors, &relay.pubkey_hex(), &verifier).is_err());
        }
        let too_many = json!([
            presence_event(&relay, "online", p(&a)),
            presence_event(&relay, "online", p(&b))
        ]);
        assert!(parse_snapshot(&too_many, &[a], &relay.pubkey_hex(), &verifier).is_err());
    }

    #[test]
    fn own_publication_updates_our_evidence() {
        let viewer = hex(0);
        let mut directory = connected(&viewer, std::slice::from_ref(&viewer));
        directory.accepted(PresenceState::Away, 10);
        assert_eq!(
            directory.snapshot().get(&viewer),
            Some(&PresenceState::Away)
        );
    }

    #[test]
    fn publisher_renews_with_jitter_and_reacts_to_idle() {
        let mut publisher = PresencePublisher::default();
        assert!(publisher.due(0).is_none());
        publisher.set_connected(true, 0);
        let first = publisher.deadline().unwrap();
        assert!((250..1_000).contains(&first));
        let publication = publisher.due(first).unwrap();
        assert_eq!(publication.state, PresenceState::Online);
        assert_eq!(publisher.deadline(), None);
        let accepted = publisher.finished(publication, PublishOutcome::Accepted, first + 10);
        assert_eq!(accepted, Some(PresenceState::Online));
        let renewal = publisher.deadline().unwrap() - (first + 10);
        assert!((60_000..65_000).contains(&renewal));

        // Idle: away goes out at once, behind the five-second send gate.
        publisher.set_idle(true, first + 1_000);
        assert_eq!(publisher.deadline(), Some(first + SEND_GATE_MS));
        let publication = publisher.due(first + SEND_GATE_MS).unwrap();
        assert_eq!(publication.state, PresenceState::Away);
    }

    #[test]
    fn a_newer_intent_drops_the_old_result() {
        let mut publisher = PresencePublisher::default();
        publisher.set_connected(true, 0);
        let publication = publisher.due(1_000).unwrap();
        publisher.set_mode(PresenceMode::Away, 1_100);
        assert_eq!(
            publisher.finished(publication, PublishOutcome::Accepted, 1_200),
            None
        );
        assert_eq!(publisher.due(6_000).unwrap().state, PresenceState::Away);
    }

    #[test]
    fn accepted_offline_stops_renewal() {
        let mut publisher = PresencePublisher::default();
        publisher.set_mode(PresenceMode::Offline, 0);
        publisher.set_connected(true, 0);
        let publication = publisher.due(1_000).unwrap();
        assert_eq!(publication.state, PresenceState::Offline);
        publisher.finished(publication, PublishOutcome::Accepted, 1_100);
        assert_eq!(publisher.deadline(), None);
        // Back online resumes.
        publisher.set_mode(PresenceMode::Auto, 2_000);
        assert_eq!(publisher.deadline(), Some(6_000));
    }

    #[test]
    fn refusals_wait_a_minute_and_cooldowns_their_length() {
        let mut publisher = PresencePublisher::default();
        publisher.set_connected(true, 0);
        let publication = publisher.due(1_000).unwrap();
        publisher.finished(publication, PublishOutcome::Unconfirmed, 1_000);
        assert!((61_000..=66_000).contains(&publisher.deadline().unwrap()));

        // The shared cooldown, not the refusal, decides how long a quota
        // pause lasts.
        let publication = publisher.due(70_000).unwrap();
        publisher.finished(publication, PublishOutcome::Cooling(7_000), 70_000);
        assert_eq!(publisher.deadline(), Some(77_050));

        let publication = publisher.due(80_000).unwrap();
        publisher.finished(publication, PublishOutcome::NotSent, 80_000);
        let retry = publisher.deadline().unwrap() - 80_000;
        assert!((5_000..6_000).contains(&retry));
    }

    #[test]
    fn disconnect_stops_publishing() {
        let mut publisher = PresencePublisher::default();
        publisher.set_connected(true, 0);
        let publication = publisher.due(1_000).unwrap();
        publisher.set_connected(false, 1_100);
        assert_eq!(
            publisher.finished(publication, PublishOutcome::Accepted, 1_200),
            None
        );
        assert_eq!(publisher.deadline(), None);
    }
}
