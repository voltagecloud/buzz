//! The durable outbox (`outbox.ts`, `outbox-storage.ts`, `receipt.ts`,
//! `acceptPublish`). See `docs/port/relay.md` §3.7, §6 and §9.2.
//!
//! One outbox per community session owns pending delivery, a bounded
//! journal of confirmed operations and one change stream for every view.
//!
//! - [`Outbox::send`] builds the final event (time, `client-id`, `ms`, ID)
//!   synchronously and shows it at once as `sending`.
//! - The unsigned intent, with its final ID, is committed before signing;
//!   the signed event is committed before publishing (§9.2 #5).
//! - At most 3 deliveries run. Each has a 10 s deadline counted from
//!   enqueue that covers queue wait, persistence and signing (§9.2 #2).
//! - Only proven non-delivery is `failed`. Anything uncertain after the
//!   publisher was entered is `unknown`, and a failed retry never
//!   downgrades `unknown`, `accepted` or `seen` (§9.2 #6).
//! - A retry republishes the saved signed event. Nothing is re-signed or
//!   re-dated, and nothing restored from the journal is published until
//!   the user retries it.
//! - A verified echo ends its attempt and frees the slot even if the
//!   publisher never answers (§9.2 #1).
//!
//! I/O goes through [`Publisher`] and [`Journal`]; wall time through
//! [`Clock`]. Deadlines and polling use `tokio::time`, so tests run on
//! paused time.

use crate::event::{Verified, kinds};
use crate::fold::MessageClock;
use crate::http::{HttpError, RelayHttp};
use crate::messages::Draft;
use crate::reader::{Priority, ReadOptions, Reader};
use crate::socket::filter;
use buzz_identity::{Identity, IdentityError, Purpose};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use nostr::prelude::{Event, EventId, PublicKey, UnsignedEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{oneshot, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

pub const MAX_PENDING: usize = 256;
pub const MAX_CONFIRMED: usize = 2048;
const MAX_CONFIRMED_BYTES: usize = 8 << 20;
const MAX_PENDING_BYTES: usize = 2 << 20;
const MAX_SAVED_BYTES: usize = 10 << 20;
/// Largest `{kind, content, tags}` a caller may submit (`budget.ts`).
pub const MAX_INPUT_BYTES: usize = 32 << 10;
/// Receipts are read with this cap and strict UTF-8 (`receipt.ts`).
pub const MAX_RECEIPT_BYTES: usize = 16 << 10;
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
const SLOTS: usize = 3;
/// Echo reads after the first: 0, +500, +1500, +4000, +10000 ms (§6.4).
const CONFIRM_DELAYS: [Duration; 4] = [
    Duration::from_millis(500),
    Duration::from_millis(1_500),
    Duration::from_millis(4_000),
    Duration::from_millis(10_000),
];
/// The relay refuses events outside ±900 s before it deduplicates.
const OLD_EVENT_SECS: u64 = 15 * 60;
const MAX_EDIT_LEAD_SECS: u64 = 60;
/// Product feedback lands in a private sidecar and is never echoed.
const PRODUCT_FEEDBACK: u16 = 42000;
const HYDRATION_BATCH: usize = 12;

const TIMED_OUT: &str = "Delivery timed out; check or retry this message";
const RECOVER_FIRST: &str = "Recover the earlier message before sending another";
const WORKFLOW_UNCERTAIN: &str = "Workflow delivery could not be confirmed; inspect recent activity before submitting another command.";
const WORKFLOW_REJECTED: &str =
    "Workflow command rejected; retain the draft and refresh the saved configuration.";
const TOO_OLD: &str = "This event is too old to retry and was not found on the relay. Check the conversation before sending it again.";
const INVALID_RECEIPT: &str = "Relay returned an invalid delivery receipt";

// ---------------------------------------------------------------------------
// Records

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    Sending,
    Accepted,
    Unknown,
    Failed,
    Seen,
}

/// Caller-owned state committed atomically with its operation, such as a
/// New-message draft. The key is unique among pending operations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recovery {
    pub key: String,
    pub value: String,
}

impl Recovery {
    fn validate(&self) -> Result<(), OutboxError> {
        let key_len = self.key.encode_utf16().count();
        let bytes = serde_json::to_string(self).map_or(usize::MAX, |json| json.len());
        if key_len == 0 || key_len > 200 || bytes > 64 << 10 {
            return Err(OutboxError::Refused("Invalid outbox recovery state".into()));
        }
        Ok(())
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One outgoing operation, persisted verbatim (§6.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// The unsigned template with its final ID.
    pub event: UnsignedEvent,
    /// The exact signed event, once signed. Never regenerated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed: Option<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<Recovery>,
    /// The caller verified the domain outcome, not just delivery.
    #[serde(default, skip_serializing_if = "is_false")]
    pub acknowledged: bool,
    /// Caller-scoped admission: a retry needs a live gate.
    #[serde(default, skip_serializing_if = "is_false")]
    pub guarded: bool,
    pub delivery: Delivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Operation {
    pub fn id(&self) -> EventId {
        self.event
            .id
            .expect("outbox operations carry their final ID")
    }

    pub fn kind(&self) -> u16 {
        self.event.kind.as_u16()
    }

    pub fn created_at(&self) -> u64 {
        self.event.created_at.as_secs()
    }

    /// The first value of the first tag with this name.
    pub fn tag(&self, name: &str) -> Option<&str> {
        self.event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .find(|tag| tag.first().map(String::as_str) == Some(name))
            .and_then(|tag| tag.get(1))
            .map(String::as_str)
    }

    pub fn channel(&self) -> Option<&str> {
        self.tag("h")
    }

    pub fn has_tag(&self, name: &str, value: &str) -> bool {
        self.event.tags.iter().any(|tag| {
            let tag = tag.as_slice();
            tag.first().map(String::as_str) == Some(name)
                && tag.get(1).map(String::as_str) == Some(value)
        })
    }

    fn bytes(&self) -> usize {
        serde_json::to_vec(self).map_or(0, |json| json.len())
    }

    /// Confirmed, without caller recovery: moves to retained history.
    fn complete(&self) -> bool {
        (self.kind() == kinds::PUT_USER && self.acknowledged) || self.delivery == Delivery::Seen
    }
}

/// Workflow commands await their receipt even after an echo and are never
/// retried (§9.2 #22).
pub fn is_workflow_operation(event: &UnsignedEvent) -> bool {
    match event.kind.as_u16() {
        30620 | 46020 => true,
        kinds::DELETION => event.tags.iter().any(|tag| {
            let tag = tag.as_slice();
            tag.first().map(String::as_str) == Some("a")
                && tag.get(1).is_some_and(|value| {
                    // The relay parses the kind numerically, `+` and zeros included.
                    let value = value.strip_prefix('+').unwrap_or(value);
                    value.trim_start_matches('0').starts_with("30620:")
                })
        }),
        _ => false,
    }
}

fn purpose(kind: u16) -> Purpose {
    if kind == kinds::APP_DATA {
        Purpose::AppData
    } else {
        Purpose::Content
    }
}

// ---------------------------------------------------------------------------
// Errors and publication

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OutboxError {
    #[error("Relay session closed")]
    Closed,
    #[error("Channel addition cancelled")]
    Cancelled,
    /// The journal could not be loaded or committed.
    #[error("{0}")]
    Storage(String),
    #[error("{0}")]
    Refused(String),
}

/// How a publication ended when it did not succeed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    /// Proven not delivered: the operation becomes `failed`.
    #[error("{0}")]
    Rejected(String),
    /// May have reached the relay: the operation becomes `unknown`.
    #[error("{0}")]
    Uncertain(String),
}

/// Maps a `POST /events` result to an outcome (§3.7, `acceptPublish`).
/// `accepted:false` with a `duplicate:` reason counts as accepted: the
/// relay already holds the event (§9.3 #3).
pub fn accept_publish(
    result: Result<Vec<u8>, HttpError>,
    id: &EventId,
) -> Result<String, PublishError> {
    use PublishError::{Rejected, Uncertain};
    let body = match result {
        Ok(body) => body,
        Err(HttpError::Status { status: 409, .. }) => {
            return Err(Rejected(
                "conflict: the relay state changed; reload before writing again".into(),
            ));
        }
        Err(HttpError::Status {
            status: status @ (400 | 401 | 403 | 404 | 413 | 422),
            ..
        }) => return Err(Rejected(format!("Relay rejected the message ({status})"))),
        // Only proven non-delivery fails a send: an API quota refusal, or
        // the relay's own `sent: false`.
        Err(HttpError::Status {
            status,
            message,
            not_sent,
            ..
        }) => {
            if status == 429
                && let Some(seconds) = crate::http::api_quota(&message)
            {
                return Err(Rejected(format!(
                    "rate-limited: quota exceeded; retry in {seconds}s"
                )));
            }
            if not_sent {
                return Err(Rejected(crate::http::api_failure_text(status, &message)));
            }
            return Err(Uncertain(format!(
                "Relay delivery could not be confirmed ({status})"
            )));
        }
        Err(error) if error.not_sent() => return Err(Rejected(error.to_string())),
        Err(_) => return Err(Uncertain("Relay request could not be confirmed".into())),
    };
    if body.len() > MAX_RECEIPT_BYTES {
        return Err(Uncertain(
            "Relay delivery receipt exceeds the size limit".into(),
        ));
    }
    let receipt: Value = std::str::from_utf8(&body)
        .ok()
        .and_then(|text| serde_json::from_str(text).ok())
        .ok_or_else(|| Uncertain(INVALID_RECEIPT.into()))?;
    let accepted = receipt.get("accepted").and_then(Value::as_bool);
    let (Some(accepted), true) = (
        accepted,
        receipt.get("event_id").and_then(Value::as_str) == Some(id.to_hex().as_str()),
    ) else {
        return Err(Uncertain(INVALID_RECEIPT.into()));
    };
    let message = receipt.get("message").and_then(Value::as_str);
    match (accepted, message) {
        (true, message) => Ok(message.unwrap_or_default().to_owned()),
        (false, Some(message)) if message.starts_with("duplicate:") => Ok(message.to_owned()),
        (false, Some(message)) => Err(Rejected(message.to_owned())),
        (false, None) => Err(Rejected("Relay rejected the message".into())),
    }
}

/// Publishes exact signed bytes. An event older than 15 minutes is looked
/// up by ID instead of re-posted: the relay would refuse it on clock drift
/// before deduplicating, so a landed send would look rejected (§9.2 #7).
pub async fn publish_event(
    publisher: &dyn Publisher,
    event: &Event,
    now_secs: u64,
) -> Result<String, PublishError> {
    if event.created_at.as_secs() + OLD_EVENT_SECS < now_secs {
        return match publisher.find_event(event).await {
            Ok(true) => Ok(String::new()),
            Ok(false) => Err(PublishError::Rejected(TOO_OLD.into())),
            Err(error) => Err(PublishError::Uncertain(error)),
        };
    }
    accept_publish(publisher.post_event(event).await, &event.id)
}

// ---------------------------------------------------------------------------
// I/O seams

/// Where signed events go.
pub trait Publisher: Send + Sync + 'static {
    /// `POST /events` with the signed event as the body. Never retried here.
    fn post_event(&self, event: &Event) -> BoxFuture<'static, Result<Vec<u8>, HttpError>>;

    /// A strong read by ID, kind and author (the 15-minute guard). `Err`
    /// carries a displayable reason.
    fn find_event(&self, event: &Event) -> BoxFuture<'static, Result<bool, String>>;

    /// The echo read `[{"ids":[id],"limit":1}]`. Failures read as nothing.
    fn read_echo(&self, id: EventId, priority: Priority) -> BoxFuture<'static, Vec<Verified>>;

    /// Read-only work before dispatch, such as the mention roster
    /// preflight. Its failure is never uncertain.
    fn prepare(&self, _event: &Event) -> BoxFuture<'static, Result<(), PublishError>> {
        Box::pin(async { Ok(()) })
    }
}

/// [`Publisher`] over the community's HTTP lane and reader.
#[derive(Clone)]
pub struct HttpPublisher {
    http: RelayHttp,
    reader: Reader,
}

impl HttpPublisher {
    pub fn new(http: RelayHttp, reader: Reader) -> Self {
        Self { http, reader }
    }
}

impl Publisher for HttpPublisher {
    fn post_event(&self, event: &Event) -> BoxFuture<'static, Result<Vec<u8>, HttpError>> {
        let http = self.http.clone();
        let body = serde_json::to_string(event).expect("events serialize");
        Box::pin(async move { http.post("/events", body).await })
    }

    fn find_event(&self, event: &Event) -> BoxFuture<'static, Result<bool, String>> {
        let reader = self.reader.clone();
        let id = event.id;
        let query = filter(json!({
            "kinds": [event.kind.as_u16()],
            "ids": [id.to_hex()],
            "authors": [event.pubkey.to_hex()],
            "limit": 1,
            "consistency": "strong",
        }));
        Box::pin(async move {
            let options = ReadOptions {
                fresh: true,
                ..ReadOptions::default()
            };
            reader
                .read(vec![query], options)
                .await
                .map(|events| events.iter().any(|event| event.id == id))
                .map_err(|error| error.message)
        })
    }

    fn read_echo(&self, id: EventId, priority: Priority) -> BoxFuture<'static, Vec<Verified>> {
        let reader = self.reader.clone();
        let query = filter(json!({"ids": [id.to_hex()], "limit": 1}));
        Box::pin(async move {
            let options = ReadOptions {
                priority,
                fresh: false,
            };
            reader
                .read(vec![query], options)
                .await
                .map(|events| events.to_vec())
                .unwrap_or_default()
        })
    }
}

/// The persisted journal. Each commit replaces the whole journal
/// atomically; the SQLite store may diff rows inside one transaction.
pub trait Journal: Send + Sync + 'static {
    fn load(&self) -> BoxFuture<'static, Result<Vec<Operation>, String>>;
    fn commit(&self, operations: Vec<Operation>) -> BoxFuture<'static, Result<(), String>>;
}

/// A journal that lives only in memory.
#[derive(Clone, Default)]
pub struct MemoryJournal {
    records: Arc<Mutex<Vec<Operation>>>,
}

impl MemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn records(&self) -> Vec<Operation> {
        self.records.lock().expect("journal lock").clone()
    }
}

impl Journal for MemoryJournal {
    fn load(&self) -> BoxFuture<'static, Result<Vec<Operation>, String>> {
        let records = self.records();
        Box::pin(async move { Ok(records) })
    }

    fn commit(&self, operations: Vec<Operation>) -> BoxFuture<'static, Result<(), String>> {
        *self.records.lock().expect("journal lock") = operations;
        Box::pin(async { Ok(()) })
    }
}

/// Wall time, for `created_at`, `ms` tags and the 15-minute guard.
pub trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64)
    }
}

/// A live admission check for a caller-scoped operation (an invitation).
pub type Gate = Arc<dyn Fn() -> bool + Send + Sync>;
/// Receives a command's relay message, or `None` when it could not be
/// confirmed. Called for workflow operations only.
pub type ReceiptListener = Arc<dyn Fn(EventId, Option<String>) + Send + Sync>;
/// Work a send observer wants done once that send is delivered, with the
/// other observed sends still undelivered.
pub type DeliveryWork = Box<dyn FnOnce(&[UnsignedEvent]) + Send>;
/// Sees every fresh send as it is queued (`observeSend`), never a restored
/// one, and may return work for its delivery. It cannot affect delivery.
pub type SendObserver = Arc<dyn Fn(&UnsignedEvent) -> Option<DeliveryWork> + Send + Sync>;
type Keep = Arc<dyn Fn(&Operation) -> bool + Send + Sync>;
type SharedResult = Shared<BoxFuture<'static, Result<(), OutboxError>>>;

pub struct OutboxOptions {
    pub delivery_timeout: Duration,
    pub clock: Arc<dyn Clock>,
    /// The session's send clock, shared with its rendered views.
    pub message_clock: Arc<MessageClock>,
    pub on_receipt: Option<ReceiptListener>,
}

impl Default for OutboxOptions {
    fn default() -> Self {
        Self {
            delivery_timeout: DELIVERY_TIMEOUT,
            clock: Arc::new(SystemClock),
            message_clock: Arc::new(MessageClock::new()),
            on_receipt: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct SendOptions {
    pub recovery: Option<Recovery>,
    /// Makes the operation guarded: checked now, before signing and at
    /// publisher entry, and required again for any retry.
    pub gate: Option<Gate>,
}

// ---------------------------------------------------------------------------
// The outbox

/// A cheap handle; clones share one outbox.
#[derive(Clone)]
pub struct Outbox {
    inner: Arc<Inner>,
}

struct Inner {
    viewer: PublicKey,
    identity: Arc<Identity>,
    publisher: Arc<dyn Publisher>,
    journal: Arc<dyn Journal>,
    options: OutboxOptions,
    state: Mutex<State>,
    /// Serializes commits, like the reference's `durable` promise chain.
    durable: tokio::sync::Mutex<()>,
    /// `true` once hydration finished, either way.
    hydration: watch::Sender<bool>,
    changes: watch::Sender<u64>,
    observers: Mutex<Vec<(u64, SendObserver)>>,
    /// The runtime `new` ran on. Callers may reach the outbox from threads
    /// with no Tokio context (GPUI), so background work spawns through it.
    runtime: tokio::runtime::Handle,
}

/// Stops a send observer when dropped.
pub struct SendObservation {
    inner: std::sync::Weak<Inner>,
    id: u64,
}

impl Drop for SendObservation {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .observers
                .lock()
                .expect("observer lock")
                .retain(|(id, _)| *id != self.id);
        }
    }
}

#[derive(Default)]
struct State {
    pending: Vec<Arc<Operation>>,
    completed: Completed,
    attempts: HashMap<EventId, Attempt>,
    dismissing: HashMap<EventId, SharedResult>,
    gates: HashMap<EventId, Gate>,
    confirming: HashMap<EventId, AbortHandle>,
    closed: bool,
    hydrated: bool,
    storage_error: Option<String>,
    /// Set by a revocation; fences confirmed records still being loaded.
    purge: Option<Keep>,
    serial: u64,
    revision: u64,
    pending_view: Arc<[Arc<Operation>]>,
    visible_view: Arc<[Arc<Operation>]>,
    /// Observer work waiting for each observed send's delivery.
    delivery_work: HashMap<EventId, (UnsignedEvent, Vec<DeliveryWork>)>,
    next_observer: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Queued,
    Running,
    /// The publisher was entered: the outcome may now be uncertain.
    Publishing,
}

struct Attempt {
    serial: u64,
    /// A failed retry cannot disprove an earlier dispatched attempt.
    previous: Delivery,
    intent: Option<SharedResult>,
    phase: Phase,
    start: Option<oneshot::Sender<()>>,
    task: AbortHandle,
}

struct Failure {
    message: String,
    rejected: bool,
}

impl Failure {
    fn before(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
            rejected: false,
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    Save,
    Dismiss(EventId),
    Acknowledge(EventId),
}

/// Confirmed operations, oldest first, bounded by count and bytes.
#[derive(Default)]
struct Completed {
    items: VecDeque<(Arc<Operation>, usize)>,
    bytes: usize,
}

impl Completed {
    fn set(&mut self, operation: Arc<Operation>) {
        self.delete(&operation.id());
        let bytes = operation.bytes();
        if bytes > MAX_CONFIRMED_BYTES {
            return;
        }
        self.items.push_back((operation, bytes));
        self.bytes += bytes;
        while self.items.len() > MAX_CONFIRMED || self.bytes > MAX_CONFIRMED_BYTES {
            let Some((_, bytes)) = self.items.pop_front() else {
                break;
            };
            self.bytes -= bytes;
        }
    }

    fn delete(&mut self, id: &EventId) -> Option<Arc<Operation>> {
        let index = self.items.iter().position(|(op, _)| op.id() == *id)?;
        let (operation, bytes) = self.items.remove(index)?;
        self.bytes -= bytes;
        Some(operation)
    }

    fn iter(&self) -> impl Iterator<Item = &Arc<Operation>> {
        self.items.iter().map(|(op, _)| op)
    }

    fn retain(&mut self, keep: impl Fn(&Operation) -> bool) -> bool {
        let before = self.items.len();
        self.items.retain(|(op, _)| keep(op));
        self.bytes = self.items.iter().map(|(_, bytes)| bytes).sum();
        self.items.len() != before
    }
}

impl State {
    fn find(&self, id: &EventId) -> Option<&Arc<Operation>> {
        self.pending.iter().find(|op| op.id() == *id)
    }

    fn visible(&self) -> impl Iterator<Item = &Arc<Operation>> {
        self.completed.iter().chain(self.pending.iter())
    }

    fn find_visible(&self, id: &EventId) -> Option<&Arc<Operation>> {
        self.visible().find(|op| op.id() == *id)
    }

    fn replace(&mut self, operation: Operation) {
        let id = operation.id();
        if let Some(slot) = self.pending.iter_mut().find(|op| op.id() == id) {
            *slot = Arc::new(operation);
        }
    }

    fn remove_pending(&mut self, id: &EventId) {
        self.pending.retain(|op| op.id() != *id);
    }

    fn attempt(&self, id: &EventId, serial: u64) -> Option<&Attempt> {
        self.attempts
            .get(id)
            .filter(|attempt| attempt.serial == serial)
    }

    fn live(&self, id: &EventId, serial: u64) -> bool {
        !self.closed && self.attempt(id, serial).is_some() && self.find(id).is_some()
    }
}

fn failed_delivery(previous: Delivery) -> Delivery {
    match previous {
        Delivery::Unknown | Delivery::Accepted | Delivery::Seen => previous,
        _ => Delivery::Failed,
    }
}

fn input_bytes(draft: &Draft) -> usize {
    serde_json::to_string(&json!({
        "kind": draft.kind,
        "content": draft.content,
        "tags": draft.tags,
    }))
    .map_or(usize::MAX, |json| json.len())
}

impl Outbox {
    /// Starts loading the journal in the background. Sends are accepted at
    /// once; their intent is committed after the restored journal. Must be
    /// called inside a Tokio runtime.
    pub fn new(
        identity: Arc<Identity>,
        publisher: Arc<dyn Publisher>,
        journal: Arc<dyn Journal>,
        options: OutboxOptions,
    ) -> Self {
        let inner = Arc::new(Inner {
            viewer: identity.public_key(),
            identity,
            publisher,
            journal,
            options,
            state: Mutex::new(State::default()),
            durable: tokio::sync::Mutex::new(()),
            hydration: watch::Sender::new(false),
            changes: watch::Sender::new(0),
            observers: Mutex::new(Vec::new()),
            runtime: tokio::runtime::Handle::current(),
        });
        inner.runtime.spawn(inner.clone().hydrate());
        Self { inner }
    }

    /// Resolves once the journal is loaded.
    pub async fn ready(&self) -> Result<(), OutboxError> {
        self.inner.hydrated().await;
        let state = self.inner.lock();
        if state.closed {
            return Err(OutboxError::Closed);
        }
        match &state.storage_error {
            Some(error) => Err(OutboxError::Storage(error.clone())),
            None => Ok(()),
        }
    }

    /// Outstanding operations, in submission order.
    pub fn pending(&self) -> Arc<[Arc<Operation>]> {
        self.inner.lock().pending_view.clone()
    }

    /// Confirmed retention followed by pending operations: what views fold.
    pub fn visible(&self) -> Arc<[Arc<Operation>]> {
        self.inner.lock().visible_view.clone()
    }

    pub fn get(&self, id: &EventId) -> Option<Arc<Operation>> {
        self.inner.lock().find_visible(id).cloned()
    }

    /// Bumps on every change to [`Self::pending`] or [`Self::visible`].
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    /// Calls `observer` for each fresh send (`observeSend`), such as a
    /// mention that should wake an agent; the work it returns runs once
    /// that send is accepted or echoed. Dropping the handle stops it.
    pub fn observe_send(&self, observer: SendObserver) -> SendObservation {
        let id = {
            let mut state = self.inner.lock();
            state.next_observer += 1;
            state.next_observer
        };
        self.inner
            .observers
            .lock()
            .expect("observer lock")
            .push((id, observer));
        SendObservation {
            inner: Arc::downgrade(&self.inner),
            id,
        }
    }

    /// Dates and identifies the draft, shows it as `sending`, and queues
    /// its delivery. Returns the final event ID.
    pub fn send(&self, draft: Draft, options: SendOptions) -> Result<EventId, OutboxError> {
        if options.gate.as_ref().is_some_and(|gate| !gate()) {
            return Err(OutboxError::Cancelled);
        }
        let inner = &self.inner;
        let now_ms = inner.options.clock.now_ms();
        let mut state = inner.lock();
        if state.closed {
            return Err(OutboxError::Closed);
        }
        if let Some(error) = &state.storage_error {
            return Err(OutboxError::Storage(error.clone()));
        }
        if let Some(recovery) = &options.recovery {
            recovery.validate()?;
            if !state.hydrated
                || state
                    .pending
                    .iter()
                    .any(|op| op.recovery.as_ref().is_some_and(|r| r.key == recovery.key))
            {
                return Err(OutboxError::Refused(RECOVER_FIRST.into()));
            }
        }
        let too_large = input_bytes(&draft) > MAX_INPUT_BYTES;
        if (draft.kind == kinds::CHAT && draft.content.trim().is_empty()) || too_large {
            return Err(OutboxError::Refused("Message is empty or too large".into()));
        }
        if state.pending.len() >= MAX_PENDING {
            return Err(OutboxError::Refused(
                "Too many outstanding operations; resolve or dismiss a pending operation".into(),
            ));
        }
        let tag = |name: &str| {
            draft
                .tags
                .iter()
                .find(|tag| tag.first().map(String::as_str) == Some(name))
                .and_then(|tag| tag.get(1))
                .cloned()
        };
        let channel = kinds::CHANNEL_ROW_KINDS
            .contains(&draft.kind)
            .then(|| tag("h"))
            .flatten();
        if draft.kind == kinds::EDIT && !state.hydrated {
            return Err(OutboxError::Refused(
                "Message history is still loading. Try again.".into(),
            ));
        }
        // Rendered rows carry their send order within the second; the
        // optimistic row and the signed event share this ms (§6.2).
        let ms = match &channel {
            Some(channel) => inner.options.message_clock.next(channel, now_ms),
            None => now_ms,
        };
        let mut created_at = ms / 1000;
        if draft.kind == kinds::EDIT {
            // Edits have second precision on every client. Keep this
            // device's successive edits of one target strictly increasing
            // (§9.2 #19).
            if let (Some(target), Some(channel)) = (tag("e"), tag("h")) {
                for op in state.visible() {
                    if op.kind() == kinds::EDIT
                        && op.event.pubkey == inner.viewer
                        && op.has_tag("e", &target)
                        && op.has_tag("h", &channel)
                    {
                        created_at = created_at.max(op.created_at() + 1);
                    }
                }
            }
            if created_at > ms / 1000 + MAX_EDIT_LEAD_SECS {
                return Err(OutboxError::Refused(
                    "Edits are arriving too quickly or your clock changed. Wait a moment and try again."
                        .into(),
                ));
            }
        }
        let mut tags = draft.tags;
        tags.push(vec!["client-id".into(), uuid::Uuid::new_v4().to_string()]);
        if channel.is_some() {
            tags.push(vec!["ms".into(), (ms % 1000).to_string()]);
        }
        let unsigned = inner
            .identity
            .template(
                purpose(draft.kind),
                draft.kind,
                created_at,
                tags,
                draft.content,
            )
            .map_err(|error| {
                OutboxError::Refused(match error {
                    IdentityError::NotAllowed => {
                        "This relay connection cannot publish that event kind".into()
                    }
                    IdentityError::TooLarge => "Message is empty or too large".into(),
                    other => other.to_string(),
                })
            })?;
        let id = unsigned.id.expect("templates carry their ID");
        if let Some(gate) = &options.gate {
            state.gates.insert(id, gate.clone());
        }
        // Observers see the intent before any delivery can start; one
        // failing observer never touches the others or the send.
        let observers: Vec<SendObserver> = inner
            .observers
            .lock()
            .expect("observer lock")
            .iter()
            .map(|(_, observer)| observer.clone())
            .collect();
        let work: Vec<DeliveryWork> = observers
            .iter()
            .filter_map(|observer| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer(&unsigned)))
                    .ok()
                    .flatten()
            })
            .collect();
        if !work.is_empty() {
            state.delivery_work.insert(id, (unsigned.clone(), work));
        }
        state.pending.push(Arc::new(Operation {
            event: unsigned,
            signed: None,
            recovery: options.recovery,
            acknowledged: false,
            guarded: options.gate.is_some(),
            delivery: Delivery::Sending,
            error: None,
        }));
        inner.notify(&mut state);
        drop(state);
        let intent = inner.persist(Action::Save).boxed().shared();
        inner.schedule(id, Some(intent), Delivery::Failed);
        Ok(id)
    }

    /// Republishes the saved signed event, or signs the saved template if
    /// it was never signed. A guarded operation needs a live gate.
    pub fn retry(&self, id: &EventId, gate: Option<Gate>) -> Result<(), OutboxError> {
        let inner = &self.inner;
        let (guarded, stored) = {
            let state = inner.lock();
            let Some(op) = state.find(id) else {
                return Ok(());
            };
            (op.guarded, state.gates.get(id).cloned())
        };
        if guarded && !gate.as_ref().or(stored.as_ref()).is_some_and(|gate| gate()) {
            return Err(OutboxError::Cancelled);
        }
        let mut state = inner.lock();
        let Some(op) = state.find(id).cloned() else {
            return Ok(());
        };
        if state.closed
            || (op.kind() == kinds::PUT_USER && op.acknowledged)
            || is_workflow_operation(&op.event)
            || state.attempts.contains_key(id)
            || state.dismissing.contains_key(id)
        {
            return Ok(());
        }
        if let Some(gate) = &gate {
            state.gates.insert(*id, gate.clone());
        }
        let previous = op.delivery;
        let mut retried = (*op).clone();
        retried.guarded |= gate.is_some();
        retried.delivery = Delivery::Sending;
        retried.error = None;
        state.replace(retried);
        inner.notify(&mut state);
        drop(state);
        inner.schedule(*id, None, previous);
        Ok(())
    }

    /// Removes an operation from the journal; memory follows only after
    /// the commit. A no-op while an attempt runs. Refused while it holds
    /// recovery and is not `failed`. Concurrent calls share one result.
    pub fn dismiss(
        &self,
        id: &EventId,
    ) -> impl Future<Output = Result<(), OutboxError>> + Send + 'static {
        let inner = &self.inner;
        let id = *id;
        let mut state = inner.lock();
        if let Some(pending) = state.dismissing.get(&id) {
            return pending.clone();
        }
        let refusal = state.find_visible(&id).and_then(|op| {
            (op.recovery.is_some() && op.delivery != Delivery::Failed).then(|| {
                OutboxError::Refused(if op.kind() == kinds::PUT_USER {
                    "Confirm this addition in Members before removing it".into()
                } else {
                    "Confirm this message in New message before removing it".into()
                })
            })
        });
        if state.closed || state.attempts.contains_key(&id) || refusal.is_some() {
            let result = refusal.map_or(Ok(()), Err);
            return async move { result }.boxed().shared();
        }
        let persisted = inner.persist(Action::Dismiss(id));
        let owner = inner.clone();
        let work = async move {
            let result = persisted.await;
            let mut state = owner.lock();
            if result.is_ok() {
                state.gates.remove(&id);
            }
            state.dismissing.remove(&id);
            result
        }
        .boxed()
        .shared();
        state.dismissing.insert(id, work.clone());
        inner.runtime.spawn(work.clone());
        work
    }

    /// Marks the caller's domain outcome as verified and releases its
    /// recovery once that is durable. Requires `accepted` or `seen`, except
    /// for a roster-confirmed member addition.
    pub async fn acknowledge(&self, id: &EventId) -> Result<(), OutboxError> {
        self.ready().await?;
        {
            let state = self.inner.lock();
            let Some(op) = state.find_visible(id) else {
                return Ok(());
            };
            let Some(recovery) = &op.recovery else {
                return Ok(());
            };
            let member_add =
                op.kind() == kinds::PUT_USER && recovery.key.starts_with("member-add:");
            if !member_add && !matches!(op.delivery, Delivery::Accepted | Delivery::Seen) {
                return Err(OutboxError::Refused(
                    "Confirm delivery before completing this message".into(),
                ));
            }
        }
        self.inner.persist(Action::Acknowledge(*id)).await
    }

    /// Attaches caller recovery to an existing operation, pending or
    /// confirmed, before the caller resumes it.
    pub async fn recover(&self, id: &EventId, recovery: Recovery) -> Result<(), OutboxError> {
        self.ready().await?;
        recovery.validate()?;
        {
            let mut state = self.inner.lock();
            let unavailable = || {
                OutboxError::Refused(
                    "The original operation is unavailable. Refresh before retrying.".into(),
                )
            };
            if state.dismissing.contains_key(id) {
                return Err(unavailable());
            }
            let item = state.find_visible(id).cloned().ok_or_else(unavailable)?;
            if item.acknowledged {
                return Ok(());
            }
            if item
                .recovery
                .as_ref()
                .is_some_and(|saved| *saved != recovery)
                || state.pending.iter().any(|op| {
                    op.id() != *id && op.recovery.as_ref().is_some_and(|r| r.key == recovery.key)
                })
            {
                return Err(OutboxError::Refused(RECOVER_FIRST.into()));
            }
            if state.find(id).is_none() && state.pending.len() >= MAX_PENDING {
                return Err(OutboxError::Refused(
                    "Too many outstanding operations; resolve a pending operation".into(),
                ));
            }
            // Protect it at once against dismissal and echo eviction. If the
            // commit fails the protection stays; a later recovery commits it.
            let mut retained = (*item).clone();
            retained.recovery = Some(recovery);
            state.completed.delete(id);
            state.remove_pending(id);
            state.pending.push(Arc::new(retained));
            self.inner.notify(&mut state);
        }
        self.inner.persist(Action::Save).await
    }

    /// Verified events seen in reads or live traffic. Matching pending
    /// operations become `seen`; their attempts end and free their slots.
    pub fn observe(&self, events: &[Verified]) {
        self.inner.observe(events);
    }

    /// Drops confirmed history the viewer can no longer read. Pending
    /// operations, with their IDs and signed bytes, always stay (§9.2 #9).
    /// Also fences a hydration still in flight (§9.2 #10).
    pub fn purge_confirmed(&self, keep: impl Fn(&Operation) -> bool + Send + Sync + 'static) {
        let keep: Keep = Arc::new(keep);
        let mut state = self.inner.lock();
        state.purge = Some(keep.clone());
        if state.completed.retain(|op| keep(op)) {
            self.inner.notify(&mut state);
        }
        drop(state);
        self.inner.persist_detached();
    }

    /// Stops all attempts and polling. Commits already queued still run.
    pub fn dispose(&self) {
        let mut state = self.inner.lock();
        if state.closed {
            return;
        }
        state.closed = true;
        state.gates.clear();
        for (_, attempt) in state.attempts.drain() {
            attempt.task.abort();
        }
        for (_, task) in state.confirming.drain() {
            task.abort();
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("outbox lock")
    }

    async fn hydrated(&self) {
        let mut done = self.hydration.subscribe();
        let _ = done.wait_for(|done| *done).await;
    }

    fn notify(&self, state: &mut State) {
        if state.closed {
            return;
        }
        state.pending_view = state.pending.iter().cloned().collect();
        state.visible_view = state.visible().cloned().collect();
        state.revision += 1;
        self.changes.send_replace(state.revision);
    }

    // -- persistence --------------------------------------------------------

    /// Queues a commit of the whole current journal. It runs after
    /// hydration and every earlier commit, and snapshots state only then,
    /// so it can never write back an older view (§9.2 #3). It completes
    /// even if the caller stops waiting.
    fn persist(self: &Arc<Self>, action: Action) -> BoxFuture<'static, Result<(), OutboxError>> {
        let task = self.runtime.spawn(self.clone().commit(action));
        Box::pin(async move { task.await.unwrap_or(Err(OutboxError::Closed)) })
    }

    async fn commit(self: Arc<Self>, action: Action) -> Result<(), OutboxError> {
        let _turn = self.durable.lock().await;
        self.hydrated().await;
        let records = {
            let state = self.lock();
            if let Some(error) = &state.storage_error {
                return Err(OutboxError::Storage(error.clone()));
            }
            let pending_bytes = |skip: Option<EventId>| {
                let ops: Vec<_> = state
                    .pending
                    .iter()
                    .filter(|op| Some(op.id()) != skip)
                    .collect();
                2 + ops.len().saturating_sub(1) + ops.iter().map(|op| op.bytes()).sum::<usize>()
            };
            let (records, bytes): (Vec<Operation>, usize) = match action {
                Action::Save => (
                    state.visible().map(|op| (**op).clone()).collect(),
                    pending_bytes(None),
                ),
                Action::Dismiss(id) => (
                    state
                        .visible()
                        .filter(|op| op.id() != id)
                        .map(|op| (**op).clone())
                        .collect(),
                    pending_bytes(Some(id)),
                ),
                Action::Acknowledge(id) => (
                    state
                        .visible()
                        .map(|op| {
                            let mut op = (**op).clone();
                            if op.id() == id {
                                op.recovery = None;
                                op.acknowledged = true;
                            }
                            op
                        })
                        .collect(),
                    pending_bytes(None),
                ),
            };
            if bytes > MAX_PENDING_BYTES {
                return Err(OutboxError::Storage("Outbox storage is full".into()));
            }
            records
        };
        self.journal
            .commit(records)
            .await
            .map_err(OutboxError::Storage)?;
        let mut state = self.lock();
        match action {
            Action::Save => {}
            // Removal shows only after it is durable, and before any later
            // commit can capture state.
            Action::Dismiss(id) => {
                state.completed.delete(&id);
                state.remove_pending(&id);
                // A dismissed send is never delivered: drop its work.
                state.delivery_work.remove(&id);
                self.notify(&mut state);
            }
            // Recovery is released only after its removal is durable,
            // keeping delivery evidence that arrived meanwhile.
            Action::Acknowledge(id) => {
                if let Some(current) = state.find_visible(&id).cloned() {
                    let mut cleared = (*current).clone();
                    cleared.recovery = None;
                    cleared.acknowledged = true;
                    if (cleared.delivery == Delivery::Seen || cleared.kind() == kinds::PUT_USER)
                        && !state.attempts.contains_key(&id)
                    {
                        state.remove_pending(&id);
                        state.completed.set(Arc::new(cleared));
                    } else {
                        state.replace(cleared);
                    }
                    self.notify(&mut state);
                }
            }
        }
        Ok(())
    }

    /// A commit nobody waits for.
    fn persist_detached(self: &Arc<Self>) {
        self.runtime.spawn(self.clone().commit(Action::Save));
    }

    /// A status change: shown now, committed in the background. A failed
    /// commit is fine: durable intent restores as `unknown`.
    fn save_status(self: &Arc<Self>, state: &mut State, operation: Operation) {
        state.replace(operation);
        self.notify(state);
        self.persist_detached();
    }

    // -- hydration ----------------------------------------------------------

    async fn hydrate(self: Arc<Self>) {
        let loaded = self.load().await;
        let mut state = self.lock();
        match loaded.and_then(|records| self.restore(&mut state, records)) {
            Ok(()) => state.hydrated = true,
            Err(error) => state.storage_error = Some(format!("Could not load the outbox: {error}")),
        }
        self.notify(&mut state);
        drop(state);
        self.hydration.send_replace(true);
    }

    async fn load(&self) -> Result<Vec<Operation>, String> {
        let records = self.journal.load().await?;
        let bytes: usize = records.iter().map(Operation::bytes).sum();
        if records.len() > MAX_PENDING + MAX_CONFIRMED || bytes > MAX_SAVED_BYTES {
            return Err("Saved outbox exceeds its budget".into());
        }
        // Signature checks gate the first send; yield between small
        // batches so a large journal does not stall the runtime (§9.2 #4).
        let mut decoded = Vec::with_capacity(records.len());
        let mut records = records.into_iter().peekable();
        while records.peek().is_some() {
            for record in records.by_ref().take(HYDRATION_BATCH) {
                decoded.push(self.decode(record)?);
            }
            tokio::task::yield_now().await;
        }
        Ok(decoded)
    }

    fn decode(&self, record: Operation) -> Result<Operation, String> {
        let invalid = || "Saved outbox event is invalid".to_owned();
        if let Some(signed) = &record.signed {
            signed.verify().map_err(|_| invalid())?;
        }
        let event = match &record.signed {
            Some(signed) => UnsignedEvent {
                id: Some(signed.id),
                pubkey: signed.pubkey,
                created_at: signed.created_at,
                kind: signed.kind,
                tags: signed.tags.clone(),
                content: signed.content.clone(),
            },
            None => record.event,
        };
        if event.pubkey != self.viewer || event.id.is_none() || event.verify_id().is_err() {
            return Err(invalid());
        }
        if let Some(recovery) = &record.recovery {
            recovery.validate().map_err(|error| error.to_string())?;
        }
        // Nothing restored is in flight: whatever was not proven becomes
        // `unknown`, and only an explicit retry publishes it again.
        let delivery = match (record.delivery, record.signed.is_some()) {
            (Delivery::Seen, true) => Delivery::Seen,
            (Delivery::Failed, _) => Delivery::Failed,
            _ => Delivery::Unknown,
        };
        Ok(Operation {
            event,
            delivery,
            ..record
        })
    }

    fn restore(&self, state: &mut State, restored: Vec<Operation>) -> Result<(), String> {
        let (pending, completed): (Vec<_>, Vec<_>) = restored
            .into_iter()
            .map(Arc::new)
            .partition(|op| !op.complete() || op.recovery.is_some());
        if pending.len() > MAX_PENDING {
            return Err("Saved pending outbox exceeds its budget".into());
        }
        for op in completed {
            // A revoke during hydration keeps only matching creation receipts.
            let kept = match &state.purge {
                None => true,
                Some(keep) => op.kind() == kinds::CREATE_CHANNEL && keep(&op),
            };
            if kept {
                state.completed.set(op);
            }
        }
        let sent = std::mem::take(&mut state.pending);
        state.pending = pending.into_iter().chain(sent).collect();
        Ok(())
    }

    // -- delivery -----------------------------------------------------------

    fn schedule(self: &Arc<Self>, id: EventId, intent: Option<SharedResult>, previous: Delivery) {
        let mut state = self.lock();
        if state.closed || state.attempts.contains_key(&id) {
            return;
        }
        state.serial += 1;
        let serial = state.serial;
        let (start, started) = oneshot::channel();
        // The deadline starts now and covers queue wait (§9.2 #2).
        let expires_at = Instant::now() + self.options.delivery_timeout;
        let task = self
            .runtime
            .spawn(self.clone().attempt(id, serial, expires_at, started));
        state.attempts.insert(
            id,
            Attempt {
                serial,
                previous,
                intent,
                phase: Phase::Queued,
                start: Some(start),
                task: task.abort_handle(),
            },
        );
        self.kick(&mut state);
    }

    /// Starts queued attempts, in submission order, while slots are free.
    fn kick(&self, state: &mut State) {
        let State {
            pending, attempts, ..
        } = state;
        let running = attempts
            .values()
            .filter(|attempt| attempt.phase != Phase::Queued)
            .count();
        let mut free = SLOTS.saturating_sub(running);
        for op in pending.iter() {
            if free == 0 {
                break;
            }
            if op.delivery != Delivery::Sending {
                continue;
            }
            if let Some(attempt) = attempts.get_mut(&op.id())
                && attempt.phase == Phase::Queued
                && let Some(start) = attempt.start.take()
            {
                attempt.phase = Phase::Running;
                let _ = start.send(());
                free -= 1;
            }
        }
    }

    async fn attempt(
        self: Arc<Self>,
        id: EventId,
        serial: u64,
        expires_at: Instant,
        started: oneshot::Receiver<()>,
    ) {
        let run = async {
            if started.await.is_err() {
                std::future::pending::<()>().await;
            }
            self.deliver(id, serial).await
        };
        let failure = tokio::select! {
            biased;
            () = tokio::time::sleep_until(expires_at) => Some(Failure::before(TIMED_OUT)),
            failure = run => failure,
        };
        if let Some(failure) = failure {
            self.fail(id, serial, failure);
        }
        self.finish(id, serial);
    }

    /// Lets a guarded operation through only while its gate says yes.
    fn admitted(&self, id: &EventId) -> Result<(), Failure> {
        let gate = {
            let state = self.lock();
            match state.find(id) {
                Some(op) if op.guarded => Some(state.gates.get(id).cloned()),
                _ => None,
            }
        };
        match gate {
            None => Ok(()),
            Some(Some(gate)) if gate() => Ok(()),
            Some(_) => Err(Failure::before(OutboxError::Cancelled)),
        }
    }

    /// One delivery attempt. `None` means nothing left to record: it
    /// succeeded, or the operation was confirmed or removed meanwhile.
    async fn deliver(self: &Arc<Self>, id: EventId, serial: u64) -> Option<Failure> {
        // 1. The unsigned intent is durable before signing.
        let intent = self.lock().attempt(&id, serial)?.intent.clone();
        let persisted = match intent {
            Some(intent) => intent.await,
            None => self.persist(Action::Save).await,
        };
        if let Err(error) = persisted {
            return Some(Failure::before(error));
        }
        let initial = {
            let state = self.lock();
            if !state.live(&id, serial) {
                return None;
            }
            state.find(&id).cloned()?
        };
        if let Err(failure) = self.admitted(&id) {
            return Some(failure);
        }
        // 2. Sign once. A retry reuses the saved signature.
        let signed = match &initial.signed {
            Some(signed) => signed.clone(),
            None => match self
                .identity
                .sign(purpose(initial.kind()), initial.event.clone())
            {
                Ok(signed) => signed,
                Err(error) => return Some(Failure::before(error)),
            },
        };
        if signed.id != id || signed.pubkey != self.viewer {
            return Some(Failure::before("Signer changed the outgoing event"));
        }
        debug_assert!(signed.verify().is_ok());
        // 3. The signed bytes are durable before publishing.
        {
            let mut state = self.lock();
            if !state.live(&id, serial) {
                return None;
            }
            let current = state.find(&id).cloned()?;
            if current.signed.is_none() {
                let mut updated = (*current).clone();
                updated.signed = Some(signed.clone());
                state.replace(updated);
                self.notify(&mut state);
            }
        }
        if let Err(error) = self.persist(Action::Save).await {
            return Some(Failure::before(error));
        }
        if !self.lock().live(&id, serial) {
            return None;
        }
        // 4. Read-only preparation cannot make the outcome uncertain.
        if let Err(error) = self.publisher.prepare(&signed).await {
            return Some(Failure {
                rejected: matches!(error, PublishError::Rejected(_)),
                message: error.to_string(),
            });
        }
        if !self.lock().live(&id, serial) {
            return None;
        }
        if let Err(failure) = self.admitted(&id) {
            return Some(failure);
        }
        // 5. Entering the publisher is what makes the outcome uncertain.
        {
            let mut state = self.lock();
            if !state.live(&id, serial) {
                return None;
            }
            if let Some(attempt) = state.attempts.get_mut(&id) {
                attempt.phase = Phase::Publishing;
            }
        }
        let now = self.options.clock.now_ms() / 1000;
        let result = publish_event(&*self.publisher, &signed, now).await;
        match result {
            Ok(receipt) => {
                let mut state = self.lock();
                if state.closed || state.attempt(&id, serial).is_none() {
                    return None;
                }
                let previous = state.attempt(&id, serial).map(|a| a.previous);
                let workflow = is_workflow_operation(&initial.event);
                if let Some(latest) = state.find(&id).cloned() {
                    let mut updated = (*latest).clone();
                    updated.delivery =
                        if latest.delivery == Delivery::Seen || previous == Some(Delivery::Seen) {
                            Delivery::Seen
                        } else {
                            Delivery::Accepted
                        };
                    updated.error = None;
                    self.save_status(&mut state, updated);
                }
                let work = Self::take_work(&mut state, &id);
                drop(state);
                Self::run_work(work);
                if workflow {
                    self.receipt(id, Some(receipt));
                }
                self.confirm(signed);
                None
            }
            Err(error) => Some(Failure {
                rejected: matches!(error, PublishError::Rejected(_)),
                message: error.to_string(),
            }),
        }
    }

    /// Records a failed or timed-out attempt without discarding stronger
    /// earlier evidence.
    fn fail(self: &Arc<Self>, id: EventId, serial: u64, failure: Failure) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        let Some(attempt) = state.attempt(&id, serial) else {
            return;
        };
        let (phase, previous) = (attempt.phase, attempt.previous);
        let Some(latest) = state.find(&id).cloned() else {
            return;
        };
        let workflow = is_workflow_operation(&latest.event);
        let uncertain = phase == Phase::Publishing && !failure.rejected;
        let mut updated = (*latest).clone();
        if phase == Phase::Queued {
            // Timed out before it ever started.
            updated.delivery = failed_delivery(previous);
            updated.error = Some(failure.message);
        } else {
            updated.delivery = if latest.delivery == Delivery::Seen || previous == Delivery::Seen {
                Delivery::Seen
            } else if previous == Delivery::Accepted {
                Delivery::Accepted
            } else if uncertain {
                Delivery::Unknown
            } else {
                failed_delivery(previous)
            };
            updated.error = Some(if workflow {
                if uncertain {
                    WORKFLOW_UNCERTAIN.into()
                } else {
                    WORKFLOW_REJECTED.into()
                }
            } else {
                let prefix = match previous {
                    Delivery::Unknown | Delivery::Accepted if failure.rejected => "Retry blocked: ",
                    Delivery::Unknown | Delivery::Accepted => "Retry failed: ",
                    _ => "",
                };
                format!("{prefix}{}", failure.message)
            });
        }
        let signed = latest.signed.clone();
        self.save_status(&mut state, updated);
        drop(state);
        if workflow {
            self.receipt(id, None);
        }
        // An uncertain publish may still have landed: look for its echo.
        if uncertain && let Some(signed) = signed {
            self.confirm(signed);
        }
    }

    fn finish(self: &Arc<Self>, id: EventId, serial: u64) {
        let mut state = self.lock();
        let delivery = state.find(&id).map(|op| op.delivery);
        if matches!(delivery, None | Some(Delivery::Accepted | Delivery::Seen)) {
            state.gates.remove(&id);
        }
        if state.attempt(&id, serial).is_some() {
            state.attempts.remove(&id);
        }
        if state.closed {
            return;
        }
        if let Some(op) = state.find(&id).cloned()
            && op.complete()
            && op.recovery.is_none()
        {
            state.remove_pending(&id);
            state.completed.set(op);
            self.notify(&mut state);
            self.persist_detached();
        }
        self.kick(&mut state);
    }

    fn receipt(&self, id: EventId, message: Option<String>) {
        if let Some(listener) = &self.options.on_receipt {
            listener(id, message);
        }
    }

    // -- confirmation -------------------------------------------------------

    /// Polls for the echo of an accepted or uncertain publication until it
    /// is seen (§6.4).
    fn confirm(self: &Arc<Self>, event: Event) {
        if event.kind.as_u16() == PRODUCT_FEEDBACK {
            return;
        }
        let mut state = self.lock();
        if state.closed || state.confirming.contains_key(&event.id) {
            return;
        }
        let task = self.runtime.spawn(self.clone().poll_echo(event.id));
        state.confirming.insert(event.id, task.abort_handle());
    }

    async fn poll_echo(self: Arc<Self>, id: EventId) {
        let mut delays = CONFIRM_DELAYS.into_iter();
        let mut priority = Priority::Foreground;
        loop {
            let found: Vec<Verified> = self
                .publisher
                .read_echo(id, priority)
                .await
                .into_iter()
                .filter(|event| event.id == id)
                .collect();
            if !found.is_empty() {
                self.observe(&found);
            }
            let delay = {
                let mut state = self.lock();
                if state.closed {
                    return;
                }
                let done = state
                    .find(&id)
                    .is_none_or(|op| op.delivery == Delivery::Seen);
                match delays.next() {
                    Some(delay) if !done => delay,
                    _ => {
                        state.confirming.remove(&id);
                        return;
                    }
                }
            };
            tokio::time::sleep(delay).await;
            priority = Priority::Background;
        }
    }

    fn observe(self: &Arc<Self>, events: &[Verified]) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        let by_id: HashMap<EventId, &Verified> =
            events.iter().map(|event| (event.id, event)).collect();
        let confirmed: Vec<(Arc<Operation>, Event)> = state
            .pending
            .iter()
            .filter_map(|op| {
                by_id
                    .get(&op.id())
                    .map(|event| (op.clone(), (***event).clone()))
            })
            .collect();
        if confirmed.is_empty() {
            return;
        }
        for (op, event) in &confirmed {
            let id = op.id();
            // Commands keep waiting for their receipt after an echo.
            let awaits = is_workflow_operation(&op.event) && state.attempts.contains_key(&id);
            let mut seen = (**op).clone();
            seen.signed = Some(event.clone());
            seen.delivery = Delivery::Seen;
            if awaits || op.recovery.is_some() {
                state.replace(seen);
            } else {
                state.remove_pending(&id);
                state.completed.set(Arc::new(seen));
            }
            if !awaits && let Some(attempt) = state.attempts.remove(&id) {
                // The echo ends the attempt and frees its slot even if the
                // publisher never answers (§9.2 #1).
                attempt.task.abort();
                state.gates.remove(&id);
            }
        }
        let work: Vec<_> = confirmed
            .iter()
            .map(|(op, _)| Self::take_work(&mut state, &op.id()))
            .collect();
        self.notify(&mut state);
        self.persist_detached();
        self.kick(&mut state);
        drop(state);
        for work in work {
            Self::run_work(work);
        }
    }

    /// The observer work for a delivered send, with the observed sends
    /// still waiting.
    fn take_work(
        state: &mut State,
        id: &EventId,
    ) -> Option<(Vec<DeliveryWork>, Vec<UnsignedEvent>)> {
        let (_, work) = state.delivery_work.remove(id)?;
        let pending = state
            .delivery_work
            .values()
            .map(|(event, _)| event.clone())
            .collect();
        Some((work, pending))
    }

    fn run_work(work: Option<(Vec<DeliveryWork>, Vec<UnsignedEvent>)>) {
        let Some((work, pending)) = work else {
            return;
        };
        for run in work {
            // Observers own their errors; delivery evidence never changes.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&pending)));
        }
    }
}

#[cfg(test)]
mod tests;
