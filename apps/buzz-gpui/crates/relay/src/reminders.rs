//! NIP-ER reminders (kind 30300): the viewer's self-encrypted,
//! replaceable reminder list (`reminders.ts`).
//!
//! Encryption and signing stay with the identity, through buzz-desktop's
//! guard; this owner sees only decoded intent. The relay's replaceable
//! rule picks the head of each `d` coordinate: newest `created_at`, then
//! the lowest event id.

use crate::event::kinds;
use crate::socket::Filter;
use buzz_identity::{DecodedReminder, ReminderIntent};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Records the list read asks for.
pub const LIST_LIMIT: u64 = 200;
const DAY: u64 = 86_400;

/// The message a reminder points at: Buzz's target dialect, shared with
/// beta desktop and mobile (not the NIP-ER spec shape).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReminderTarget {
    pub event_id: String,
    pub channel_id: String,
    pub preview: String,
    pub author_pubkey: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReminderStatus {
    Pending,
    Done,
    Cancelled,
}

impl ReminderStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "done" => Some(Self::Done),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reminder {
    /// The `d` coordinate.
    pub id: String,
    pub event_id: String,
    pub created_at: u64,
    /// Due time in unix seconds; absent once done or cancelled.
    pub not_before: Option<u64>,
    pub status: ReminderStatus,
    pub target: Option<ReminderTarget>,
    pub note: Option<String>,
}

impl Reminder {
    /// Beta may store empty target fields; those cannot open a message.
    pub fn navigable_target(&self) -> Option<&ReminderTarget> {
        self.target
            .as_ref()
            .filter(|target| !target.channel_id.is_empty() && !target.event_id.is_empty())
    }

    /// A pending reminder whose due time has arrived.
    pub fn is_due(&self, now: u64) -> bool {
        self.status == ReminderStatus::Pending && self.not_before.is_some_and(|at| at <= now)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemindersStatus {
    Loading,
    Ready,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemindersState {
    pub status: RemindersStatus,
    /// A history read has succeeded; local saves and live arrivals don't
    /// count.
    pub hydrated: bool,
    pub reminders: Arc<[Reminder]>,
    pub error: Option<String>,
}

impl Default for RemindersState {
    fn default() -> Self {
        Self {
            status: RemindersStatus::Loading,
            hydrated: false,
            reminders: Arc::from([]),
            error: None,
        }
    }
}

/// Mirrors the relay's validator: ASCII digits, no leading zero, a safe
/// integer.
pub fn parse_not_before(raw: Option<&str>) -> Option<u64> {
    let raw = raw?;
    let digits = !raw.is_empty()
        && raw.bytes().all(|b| b.is_ascii_digit())
        && (raw == "0" || !raw.starts_with('0'));
    raw.parse::<u64>()
        .ok()
        .filter(|value| digits && *value < (1 << 53))
}

/// One decoded record; off-shape plaintext fails closed, as NIP-ER
/// requires.
pub fn parse_reminder(
    d: &str,
    event_id: &str,
    created_at: u64,
    not_before: Option<&str>,
    content: &Value,
) -> Option<Reminder> {
    if d.is_empty() {
        return None;
    }
    let data = content.as_object()?;
    let status = ReminderStatus::parse(data.get("status")?.as_str()?)?;
    let note = match data.get("note") {
        None => None,
        Some(Value::String(note)) => Some(note.clone()),
        Some(_) => return None,
    };
    let target = match data.get("target") {
        None => None,
        Some(target) => {
            let target = target.as_object()?;
            let field = |name: &str| target.get(name)?.as_str().map(str::to_owned);
            Some(ReminderTarget {
                event_id: field("eventId")?,
                channel_id: field("channelId")?,
                preview: field("preview")?,
                author_pubkey: field("authorPubkey")?,
            })
        }
    };
    if target.is_none() && note.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    Some(Reminder {
        id: d.to_owned(),
        event_id: event_id.to_owned(),
        created_at,
        not_before: parse_not_before(not_before),
        status,
        target,
        note,
    })
}

/// The viewer's list read.
pub fn list_filter(viewer: &str) -> Filter {
    match json!({"kinds": [kinds::REMINDERS], "authors": [viewer], "limit": LIST_LIMIT}) {
        Value::Object(filter) => filter,
        _ => unreachable!("a filter is an object"),
    }
}

/// What a record's content holds.
fn content(target: Option<&ReminderTarget>, note: Option<&str>, status: ReminderStatus) -> Value {
    let mut content = serde_json::Map::new();
    if let Some(target) = target {
        content.insert("target".into(), json!(target));
    }
    if let Some(note) = note {
        content.insert("note".into(), json!(note));
    }
    content.insert("status".into(), json!(status));
    Value::Object(content)
}

fn intent(
    d: &str,
    created_at: u64,
    not_before: Option<u64>,
    expiration: Option<u64>,
    content: Value,
) -> ReminderIntent {
    serde_json::from_value(json!({
        "d": d,
        "createdAt": created_at,
        "notBefore": not_before,
        "expiration": expiration,
        "content": content,
    }))
    .expect("reminder intents have this shape")
}

/// A new pending reminder; the note is trimmed and dropped when empty.
pub fn create_intent(
    d: &str,
    now: u64,
    target: &ReminderTarget,
    not_before: u64,
    note: Option<&str>,
) -> ReminderIntent {
    let note = note.map(str::trim).filter(|note| !note.is_empty());
    intent(
        d,
        now,
        Some(not_before),
        None,
        content(Some(target), note, ReminderStatus::Pending),
    )
}

/// What an edit of an existing reminder does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReminderEdit {
    Snooze(u64),
    Complete,
    Cancel,
}

/// The replacement for `current`: built on the latest head, with
/// `created_at` strictly after it. Closed records expire in 30 to 90
/// days (`expiration_days`), as NIP-ER asks.
pub fn edit_intent(
    current: &Reminder,
    edit: ReminderEdit,
    now: u64,
    expiration_days: u64,
) -> ReminderIntent {
    let created_at = now.max(current.created_at + 1);
    let body = |status| content(current.target.as_ref(), current.note.as_deref(), status);
    let expiration = now + expiration_days.clamp(30, 89) * DAY;
    match edit {
        ReminderEdit::Snooze(at) => intent(
            &current.id,
            created_at,
            Some(at),
            None,
            body(ReminderStatus::Pending),
        ),
        ReminderEdit::Complete => intent(
            &current.id,
            created_at,
            None,
            Some(expiration),
            body(ReminderStatus::Done),
        ),
        ReminderEdit::Cancel => intent(
            &current.id,
            created_at,
            None,
            Some(expiration),
            body(ReminderStatus::Cancelled),
        ),
    }
}

/// A random 32-hex coordinate.
pub fn random_d() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A verified 30300 event's selector fields, for [`ReminderStore::accept`].
pub struct RawReminder {
    pub event_id: String,
    pub created_at: u64,
    pub d: Option<String>,
    pub not_before: Option<String>,
}

/// The list and the state of its reads (`createReminders`).
#[derive(Debug)]
pub struct ReminderStore {
    /// Heads in first-seen order.
    heads: Vec<Reminder>,
    state: RemindersState,
    /// Until the live connection is established, and again from each
    /// drop, no read can mark the list ready.
    offline: bool,
    /// Bumped by recovery, so only a read started after it can mark the
    /// list ready.
    generation: u64,
    /// The generation a read is in flight for.
    loading: Option<u64>,
}

impl Default for ReminderStore {
    fn default() -> Self {
        Self {
            heads: Vec::new(),
            state: RemindersState::default(),
            offline: true,
            generation: 0,
            loading: None,
        }
    }
}

impl ReminderStore {
    pub fn state(&self) -> &RemindersState {
        &self.state
    }

    pub fn get(&self, id: &str) -> Option<&Reminder> {
        self.heads.iter().find(|reminder| reminder.id == id)
    }

    fn publish(&mut self, status: RemindersStatus, error: Option<String>, hydrated: bool) {
        self.state = RemindersState {
            status,
            hydrated,
            reminders: Arc::from(self.heads.clone()),
            error,
        };
    }

    /// A save or live edit changes the list, not the state of the last
    /// read.
    fn publish_change(&mut self) {
        let status = match self.state.status {
            RemindersStatus::Error => RemindersStatus::Ready,
            status => status,
        };
        let (error, hydrated) = (self.state.error.clone(), self.state.hydrated);
        self.publish(status, error, hydrated);
    }

    /// The relay's replaceable rule.
    fn merge(&mut self, reminder: Reminder) -> bool {
        match self.heads.iter_mut().find(|head| head.id == reminder.id) {
            Some(current) => {
                if current.created_at > reminder.created_at
                    || (current.created_at == reminder.created_at
                        && current.event_id <= reminder.event_id)
                {
                    return false;
                }
                *current = reminder;
            }
            None => self.heads.push(reminder),
        }
        true
    }

    /// Merges decoded records; true if the list changed.
    pub fn accept(&mut self, raw: &[RawReminder], decoded: &[DecodedReminder]) -> bool {
        let mut changed = false;
        for entry in decoded {
            let Some(event) = raw.iter().find(|event| event.event_id == entry.event_id) else {
                continue;
            };
            let Some(d) = event.d.as_deref() else {
                continue;
            };
            if let Some(reminder) = parse_reminder(
                d,
                &event.event_id,
                event.created_at,
                event.not_before.as_deref(),
                &entry.content,
            ) {
                changed |= self.merge(reminder);
            }
        }
        changed
    }

    /// Live delivery from another device, or this device's own save.
    pub fn received(&mut self, raw: &[RawReminder], decoded: &[DecodedReminder]) -> bool {
        let changed = self.accept(raw, decoded);
        if changed {
            self.publish_change();
        }
        changed
    }

    /// Starts a list read unless one for this generation is in flight;
    /// returns its generation.
    pub fn begin_read(&mut self) -> Option<u64> {
        if self.loading == Some(self.generation) {
            return None;
        }
        self.loading = Some(self.generation);
        if self.state.status != RemindersStatus::Loading {
            let hydrated = self.state.hydrated;
            self.publish(RemindersStatus::Loading, None, hydrated);
        }
        Some(self.generation)
    }

    /// A list read finished. Stale generations are ignored.
    pub fn read_done(
        &mut self,
        generation: u64,
        result: Result<(Vec<RawReminder>, Vec<DecodedReminder>), String>,
    ) {
        if self.loading == Some(generation) {
            self.loading = None;
        }
        match result {
            Ok((raw, decoded)) => {
                let changed = self.accept(&raw, &decoded);
                if generation != self.generation {
                    return;
                }
                if !self.offline {
                    self.publish(RemindersStatus::Ready, None, true);
                } else if changed || !self.state.hydrated {
                    self.publish(RemindersStatus::Loading, None, true);
                }
            }
            Err(error) => {
                if generation != self.generation {
                    return;
                }
                let status = if self.heads.is_empty() {
                    RemindersStatus::Error
                } else {
                    RemindersStatus::Ready
                };
                let hydrated = self.state.hydrated;
                self.publish(status, Some(error), hydrated);
            }
        }
    }

    /// The connection dropped: the list is stale until recovery's read.
    pub fn stale(&mut self) {
        self.offline = true;
        // Loading says nothing of an earlier failure (`publishState`).
        if self.state.status != RemindersStatus::Loading {
            let hydrated = self.state.hydrated;
            self.publish(RemindersStatus::Loading, None, hydrated);
        }
    }

    /// The connection is (re-)established: only a read started now can end
    /// the outage. `read` starts one at once.
    pub fn recover(&mut self, read: bool) -> Option<u64> {
        self.offline = false;
        self.generation += 1;
        if read { self.begin_read() } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ReminderTarget {
        ReminderTarget {
            event_id: "a".repeat(64),
            channel_id: "c1".into(),
            preview: "hi".into(),
            author_pubkey: "b".repeat(64),
        }
    }

    fn raw(id: &str, created_at: u64, d: &str, not_before: Option<&str>) -> RawReminder {
        RawReminder {
            event_id: id.into(),
            created_at,
            d: Some(d.into()),
            not_before: not_before.map(str::to_owned),
        }
    }

    fn decoded(id: &str, status: &str) -> DecodedReminder {
        DecodedReminder {
            event_id: id.into(),
            content: json!({"target": target(), "status": status}),
        }
    }

    #[test]
    fn not_before_mirrors_the_relay_validator() {
        assert_eq!(parse_not_before(Some("1700000000")), Some(1_700_000_000));
        assert_eq!(parse_not_before(Some("0")), Some(0));
        for bad in ["", "01", "1e9", "-1", "9007199254740992"] {
            assert_eq!(parse_not_before(Some(bad)), None, "{bad}");
        }
    }

    #[test]
    fn off_shape_records_fail_closed() {
        let ok = parse_reminder(
            "d",
            "e",
            1,
            Some("5"),
            &json!({"note": "call", "status": "pending"}),
        );
        assert_eq!(ok.unwrap().not_before, Some(5));
        for bad in [
            json!({"status": "pending"}),
            json!({"note": "", "status": "pending"}),
            json!({"note": 1, "status": "pending"}),
            json!({"note": "x", "status": "later"}),
            json!({"target": {"eventId": "x"}, "status": "pending"}),
            json!("text"),
        ] {
            assert!(parse_reminder("d", "e", 1, None, &bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn newest_head_wins_then_the_lowest_id() {
        let mut store = ReminderStore::default();
        assert!(store.received(
            &[raw("bb", 10, "d1", Some("100"))],
            &[decoded("bb", "pending")]
        ));
        // Older loses, same time with a higher id loses.
        assert!(!store.received(&[raw("aa", 9, "d1", None)], &[decoded("aa", "done")]));
        assert!(!store.received(&[raw("cc", 10, "d1", None)], &[decoded("cc", "done")]));
        // Same time, lower id wins.
        assert!(store.received(
            &[raw("ab", 10, "d1", Some("1"))],
            &[decoded("ab", "pending")]
        ));
        assert_eq!(store.get("d1").unwrap().event_id, "ab");
        assert!(store.received(&[raw("zz", 11, "d1", None)], &[decoded("zz", "done")]));
        assert_eq!(store.get("d1").unwrap().status, ReminderStatus::Done);
    }

    #[test]
    fn only_a_read_after_recovery_marks_the_list_ready() {
        let mut store = ReminderStore::default();
        let early = store.begin_read().unwrap();
        assert_eq!(store.begin_read(), None, "one read per generation");
        store.read_done(early, Ok((Vec::new(), Vec::new())));
        assert_eq!(store.state().status, RemindersStatus::Loading);
        assert!(store.state().hydrated);
        let after = store.recover(true).unwrap();
        store.read_done(
            after,
            Ok((
                vec![raw("e1", 1, "d1", Some("5"))],
                vec![decoded("e1", "pending")],
            )),
        );
        assert_eq!(store.state().status, RemindersStatus::Ready);
        assert_eq!(store.state().reminders.len(), 1);
        // A failure with a list keeps it and reports the error.
        store.stale();
        let next = store.recover(true).unwrap();
        store.read_done(next, Err("offline".into()));
        assert_eq!(store.state().status, RemindersStatus::Ready);
        assert_eq!(store.state().error.as_deref(), Some("offline"));
        // A read from a superseded generation changes nothing.
        let old = store.begin_read().unwrap();
        assert_eq!(store.recover(false), None);
        store.read_done(old, Err("late".into()));
        assert_eq!(store.state().error, None);
    }

    #[test]
    fn edits_build_on_the_head() {
        let current = Reminder {
            id: "0123456789abcdef0123456789abcdef".into(),
            event_id: "e".into(),
            created_at: 1_000,
            not_before: Some(2_000),
            status: ReminderStatus::Pending,
            target: Some(target()),
            note: Some("call".into()),
        };
        let now = 900;
        let value = serde_json::to_value(ReminderIntentView::from(edit_intent(
            &current,
            ReminderEdit::Complete,
            now,
            45,
        )))
        .unwrap();
        assert_eq!(value["createdAt"], 1_001);
        assert_eq!(value["expiration"], now + 45 * DAY);
        assert_eq!(value["content"]["status"], "done");
        assert_eq!(value["content"]["note"], "call");
        let created = create_intent("d", 5, &target(), 10, Some("  "));
        let value = serde_json::to_value(ReminderIntentView::from(created)).unwrap();
        assert!(value["content"].get("note").is_none());
        assert_eq!(value["notBefore"], 10);
    }

    /// The intent's fields, for assertions.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ReminderIntentView {
        created_at: u64,
        not_before: Option<u64>,
        expiration: Option<u64>,
        content: Value,
    }

    impl From<ReminderIntent> for ReminderIntentView {
        fn from(intent: ReminderIntent) -> Self {
            Self {
                created_at: intent.created_at,
                not_before: intent.not_before,
                expiration: intent.expiration,
                content: intent.content,
            }
        }
    }
}
