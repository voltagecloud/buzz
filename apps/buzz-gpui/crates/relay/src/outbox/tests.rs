//! Outbox behavior, ported from `outbox.test.ts`, `outbox-recovery.test.ts`,
//! `outbox-receipts.test.ts` and the publish mapping in `transport.ts`.
//! Everything runs on paused tokio time.

use super::*;
use crate::event::Verifier;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const NOW_MS: u64 = 1_790_000_000_000;
const NOW: u64 = NOW_MS / 1000;

// ---------------------------------------------------------------------------
// Fakes

struct ManualClock(AtomicU64);

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Accept,
    Hold,
    Reject(&'static str),
    Lose,
}

struct Held {
    event: Event,
    reply: oneshot::Sender<Result<Vec<u8>, HttpError>>,
}

impl Held {
    fn respond(self, result: Result<Vec<u8>, HttpError>) {
        let _ = self.reply.send(result);
    }

    fn accept(self) {
        let body = receipt(&self.event.id, true, "");
        self.respond(Ok(body));
    }

    fn reject(self, message: &str) {
        let body = receipt(&self.event.id, false, message);
        self.respond(Ok(body));
    }

    /// The request went out but the answer never came back.
    fn lose(self) {
        self.respond(Err(HttpError::Transport("connection reset".into())));
    }
}

struct RelayState {
    mode: Mode,
    held: VecDeque<Held>,
    posted: Vec<Event>,
    stored: Vec<Event>,
    echo_reads: Vec<(EventId, Priority, Instant)>,
    lookups: usize,
    lookup_error: Option<String>,
    prepare_gate: Option<oneshot::Receiver<()>>,
}

#[derive(Clone)]
struct FakeRelay(Arc<Mutex<RelayState>>);

impl FakeRelay {
    fn new(mode: Mode) -> Self {
        Self(Arc::new(Mutex::new(RelayState {
            mode,
            held: VecDeque::new(),
            posted: Vec::new(),
            stored: Vec::new(),
            echo_reads: Vec::new(),
            lookups: 0,
            lookup_error: None,
            prepare_gate: None,
        })))
    }

    fn state(&self) -> MutexGuard<'_, RelayState> {
        self.0.lock().unwrap()
    }

    fn set_mode(&self, mode: Mode) {
        self.state().mode = mode;
    }

    fn posted(&self) -> Vec<Event> {
        self.state().posted.clone()
    }

    fn store(&self, event: &Event) {
        self.state().stored.push(event.clone());
    }

    fn take(&self) -> Held {
        self.state().held.pop_front().expect("a held publication")
    }

    fn echo_reads(&self) -> Vec<(EventId, Priority, Instant)> {
        self.state().echo_reads.clone()
    }

    /// The next `prepare` waits until the returned sender fires.
    fn hold_prepare(&self) -> oneshot::Sender<()> {
        let (release, gate) = oneshot::channel();
        self.state().prepare_gate = Some(gate);
        release
    }
}

fn receipt(id: &EventId, accepted: bool, message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"event_id": id.to_hex(), "accepted": accepted, "message": message}))
        .unwrap()
}

impl Publisher for FakeRelay {
    fn post_event(&self, event: &Event) -> BoxFuture<'static, Result<Vec<u8>, HttpError>> {
        let mut state = self.state();
        state.posted.push(event.clone());
        let id = event.id;
        match state.mode {
            Mode::Accept => {
                let body = receipt(&id, true, "");
                Box::pin(async move { Ok(body) })
            }
            Mode::Reject(message) => {
                let body = receipt(&id, false, message);
                Box::pin(async move { Ok(body) })
            }
            Mode::Lose => Box::pin(async { Err(HttpError::Transport("connection reset".into())) }),
            Mode::Hold => {
                let (reply, answer) = oneshot::channel();
                state.held.push_back(Held {
                    event: event.clone(),
                    reply,
                });
                Box::pin(async move {
                    match answer.await {
                        Ok(result) => result,
                        Err(_) => std::future::pending().await,
                    }
                })
            }
        }
    }

    fn find_event(&self, event: &Event) -> BoxFuture<'static, Result<bool, String>> {
        let mut state = self.state();
        state.lookups += 1;
        let result = match &state.lookup_error {
            Some(error) => Err(error.clone()),
            None => Ok(state.stored.iter().any(|stored| stored.id == event.id)),
        };
        Box::pin(async move { result })
    }

    fn read_echo(&self, id: EventId, priority: Priority) -> BoxFuture<'static, Vec<Verified>> {
        let mut state = self.state();
        state.echo_reads.push((id, priority, Instant::now()));
        let found: Vec<Verified> = state
            .stored
            .iter()
            .filter(|event| event.id == id)
            .map(verified)
            .collect();
        Box::pin(async move { found })
    }

    fn prepare(&self, _event: &Event) -> BoxFuture<'static, Result<(), PublishError>> {
        let gate = self.state().prepare_gate.take();
        Box::pin(async move {
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            Ok(())
        })
    }
}

#[derive(Default)]
struct JournalState {
    records: Mutex<Vec<Operation>>,
    commits: Mutex<Vec<Vec<Operation>>>,
    fail: Mutex<Option<String>>,
    fail_once: Mutex<Option<String>>,
    load_gate: Mutex<Option<oneshot::Receiver<()>>>,
    commit_gate: Mutex<Option<oneshot::Receiver<Result<(), String>>>>,
}

#[derive(Clone, Default)]
struct TestJournal(Arc<JournalState>);

impl TestJournal {
    fn with(records: Vec<Operation>) -> Self {
        let journal = Self::default();
        *journal.0.records.lock().unwrap() = records;
        journal
    }

    fn records(&self) -> Vec<Operation> {
        self.0.records.lock().unwrap().clone()
    }

    fn set_records(&self, records: Vec<Operation>) {
        *self.0.records.lock().unwrap() = records;
    }

    fn commits(&self) -> Vec<Vec<Operation>> {
        self.0.commits.lock().unwrap().clone()
    }

    fn fail(&self, error: Option<&str>) {
        *self.0.fail.lock().unwrap() = error.map(str::to_owned);
    }

    fn fail_once(&self, error: &str) {
        *self.0.fail_once.lock().unwrap() = Some(error.to_owned());
    }

    fn hold_load(&self) -> oneshot::Sender<()> {
        let (release, gate) = oneshot::channel();
        *self.0.load_gate.lock().unwrap() = Some(gate);
        release
    }

    /// The next commit waits for the sender, then fails with its error.
    fn hold_commit(&self) -> oneshot::Sender<Result<(), String>> {
        let (release, gate) = oneshot::channel();
        *self.0.commit_gate.lock().unwrap() = Some(gate);
        release
    }
}

impl Journal for TestJournal {
    fn load(&self) -> BoxFuture<'static, Result<Vec<Operation>, String>> {
        let gate = self.0.load_gate.lock().unwrap().take();
        let state = self.0.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            Ok(state.records.lock().unwrap().clone())
        })
    }

    fn commit(&self, operations: Vec<Operation>) -> BoxFuture<'static, Result<(), String>> {
        let gate = self.0.commit_gate.lock().unwrap().take();
        let state = self.0.clone();
        Box::pin(async move {
            if let Some(gate) = gate
                && let Ok(Err(error)) = gate.await
            {
                return Err(error);
            }
            if let Some(error) = state.fail_once.lock().unwrap().take() {
                return Err(error);
            }
            if let Some(error) = state.fail.lock().unwrap().clone() {
                return Err(error);
            }
            state.commits.lock().unwrap().push(operations.clone());
            *state.records.lock().unwrap() = operations;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Harness

type Receipts = Arc<Mutex<Vec<(EventId, Option<String>)>>>;

struct Harness {
    outbox: Outbox,
    relay: FakeRelay,
    journal: TestJournal,
    identity: Arc<Identity>,
    clock: Arc<ManualClock>,
    receipts: Receipts,
}

impl Harness {
    fn with(
        identity: Arc<Identity>,
        journal: TestJournal,
        relay: FakeRelay,
        timeout: Duration,
    ) -> Self {
        let clock = Arc::new(ManualClock(AtomicU64::new(NOW_MS)));
        let receipts = Arc::new(Mutex::new(Vec::new()));
        let log = receipts.clone();
        let outbox = Outbox::new(
            identity.clone(),
            Arc::new(relay.clone()),
            Arc::new(journal.clone()),
            OutboxOptions {
                delivery_timeout: timeout,
                clock: clock.clone(),
                message_clock: Arc::new(MessageClock::new()),
                on_receipt: Some(Arc::new(move |id, message| {
                    log.lock().unwrap().push((id, message));
                })),
            },
        );
        Self {
            outbox,
            relay,
            journal,
            identity,
            clock,
            receipts,
        }
    }

    fn new(mode: Mode) -> Self {
        Self::with(
            Arc::new(Identity::ephemeral()),
            TestJournal::default(),
            FakeRelay::new(mode),
            DELIVERY_TIMEOUT,
        )
    }

    /// Another session over the same identity and journal.
    fn reopen(&self, mode: Mode) -> Self {
        self.outbox.dispose();
        Self::with(
            self.identity.clone(),
            self.journal.clone(),
            FakeRelay::new(mode),
            DELIVERY_TIMEOUT,
        )
    }

    fn send(&self, content: &str) -> EventId {
        self.outbox
            .send(chat(content), SendOptions::default())
            .unwrap()
    }

    fn op(&self, id: &EventId) -> Arc<Operation> {
        self.outbox.get(id).expect("operation is visible")
    }

    fn delivery(&self, id: &EventId) -> Delivery {
        self.op(id).delivery
    }

    fn pending_ids(&self) -> Vec<EventId> {
        self.outbox.pending().iter().map(|op| op.id()).collect()
    }

    fn signed(&self, kind: u16, created_at: u64, content: &str, tags: Vec<Vec<&str>>) -> Event {
        let unsigned = self.template(kind, created_at, content, tags);
        self.identity.sign(Purpose::Content, unsigned).unwrap()
    }

    fn template(
        &self,
        kind: u16,
        created_at: u64,
        content: &str,
        tags: Vec<Vec<&str>>,
    ) -> UnsignedEvent {
        let tags = tags
            .into_iter()
            .map(|tag| tag.into_iter().map(str::to_owned).collect())
            .collect();
        self.identity
            .template(Purpose::Content, kind, created_at, tags, content.into())
            .unwrap()
    }
}

fn chat(content: &str) -> Draft {
    Draft {
        kind: kinds::CHAT,
        content: content.into(),
        tags: vec![vec!["h".into(), "c".into()]],
    }
}

fn draft(kind: u16, tags: &[&[&str]]) -> Draft {
    Draft {
        kind,
        content: String::new(),
        tags: tags
            .iter()
            .map(|tag| tag.iter().map(|item| (*item).to_owned()).collect())
            .collect(),
    }
}

fn verified(event: &Event) -> Verified {
    Verifier::default().verify(event.clone()).unwrap()
}

fn saved(event: &Event, delivery: Delivery) -> Operation {
    Operation {
        event: UnsignedEvent {
            id: Some(event.id),
            pubkey: event.pubkey,
            created_at: event.created_at,
            kind: event.kind,
            tags: event.tags.clone(),
            content: event.content.clone(),
        },
        signed: Some(event.clone()),
        recovery: None,
        acknowledged: false,
        guarded: false,
        delivery,
        error: None,
    }
}

fn recovery() -> Recovery {
    Recovery {
        key: "new-dm".into(),
        value: r#"{"draft":"First message"}"#.into(),
    }
}

fn with_recovery() -> SendOptions {
    SendOptions {
        recovery: Some(recovery()),
        gate: None,
    }
}

/// Lets every runnable task make progress without advancing time.
async fn flush() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

// ---------------------------------------------------------------------------
// Construction and persistence order

#[tokio::test(start_paused = true)]
async fn intent_is_durable_before_signing_and_signature_before_publishing() {
    let h = Harness::new(Mode::Hold);
    h.outbox.ready().await.unwrap();
    let gate = h.journal.hold_commit();
    let id = h.send("hello");
    let op = h.op(&id);
    assert_eq!(op.delivery, Delivery::Sending);
    assert_eq!(op.created_at(), NOW);
    flush().await;
    assert!(
        h.op(&id).signed.is_none(),
        "signed before the intent was durable"
    );
    assert!(h.relay.posted().is_empty());
    gate.send(Ok(())).unwrap();
    flush().await;
    let commits = h.journal.commits();
    assert_eq!(commits[0].len(), 1);
    assert_eq!(commits[0][0].id(), id);
    assert!(commits[0][0].signed.is_none());
    let signed = commits[1][0]
        .signed
        .clone()
        .expect("signed bytes committed");
    assert_eq!(h.relay.posted(), vec![signed.clone()]);
    assert_eq!(signed.id, id);
    h.relay.take().accept();
    flush().await;
    assert_eq!(h.delivery(&id), Delivery::Accepted);
}

#[tokio::test(start_paused = true)]
async fn sends_carry_client_id_and_ms_tags_and_identical_sends_differ() {
    let h = Harness::new(Mode::Hold);
    let one = h.send("same");
    let two = h.send("same");
    assert_ne!(one, two);
    assert_eq!(h.outbox.pending().len(), 2);
    let (first, second) = (h.op(&one), h.op(&two));
    assert!(
        first
            .tag("client-id")
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
    );
    assert_ne!(first.tag("client-id"), second.tag("client-id"));
    // Same wall-clock millisecond: the send clock keeps them ordered.
    assert_eq!(first.tag("ms"), Some("0"));
    assert_eq!(second.tag("ms"), Some("1"));
    assert_eq!(first.created_at(), NOW);
    // Non-row kinds get no `ms` tag.
    let reaction = h
        .outbox
        .send(
            Draft {
                kind: kinds::REACTION,
                content: "👍".into(),
                tags: vec![
                    vec!["h".into(), "c".into()],
                    vec!["e".into(), "a".repeat(64)],
                ],
            },
            SendOptions::default(),
        )
        .unwrap();
    assert_eq!(h.op(&reaction).tag("ms"), None);
}

#[tokio::test(start_paused = true)]
async fn send_guards_refuse_bad_input() {
    let h = Harness::new(Mode::Hold);
    h.outbox.ready().await.unwrap();
    let refused =
        |result: Result<EventId, OutboxError>| matches!(result, Err(OutboxError::Refused(_)));
    assert!(refused(h.outbox.send(chat("   "), SendOptions::default())));
    assert!(refused(h.outbox.send(
        chat(&"x".repeat(MAX_INPUT_BYTES)),
        SendOptions::default()
    )));
    assert!(refused(
        h.outbox.send(draft(27235, &[]), SendOptions::default())
    ));
    let bad_recovery = SendOptions {
        recovery: Some(Recovery {
            key: String::new(),
            value: String::new(),
        }),
        gate: None,
    };
    assert!(refused(h.outbox.send(chat("x"), bad_recovery)));
    for i in 0..MAX_PENDING {
        h.send(&format!("queued {i}"));
    }
    assert!(refused(
        h.outbox.send(chat("one too many"), SendOptions::default())
    ));
    h.outbox.dispose();
    assert_eq!(
        h.outbox.send(chat("late"), SendOptions::default()),
        Err(OutboxError::Closed)
    );
}

#[tokio::test(start_paused = true)]
async fn storage_failure_never_signs_or_publishes() {
    let h = Harness::new(Mode::Accept);
    h.journal.fail(Some("disk full"));
    let id = h.send("hello");
    flush().await;
    let op = h.op(&id);
    assert_eq!(op.delivery, Delivery::Failed);
    assert_eq!(op.error.as_deref(), Some("disk full"));
    assert!(op.signed.is_none());
    assert!(h.relay.posted().is_empty());
}

// ---------------------------------------------------------------------------
// Outcomes, retry and restore

#[tokio::test(start_paused = true)]
async fn rejected_sends_stay_retryable_and_retry_reuses_the_signed_event() {
    let h = Harness::new(Mode::Hold);
    let id = h.send("hello");
    flush().await;
    h.relay.take().reject("blocked: not allowed");
    flush().await;
    let op = h.op(&id);
    assert_eq!(op.delivery, Delivery::Failed);
    assert_eq!(op.error.as_deref(), Some("blocked: not allowed"));
    h.outbox.retry(&id, None).unwrap();
    h.outbox.retry(&id, None).unwrap();
    flush().await;
    let posted = h.relay.posted();
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[0], posted[1]);
    assert_eq!(posted[0].sig, posted[1].sig);
    h.relay.take().accept();
    flush().await;
    assert_eq!(h.delivery(&id), Delivery::Accepted);
}

#[tokio::test(start_paused = true)]
async fn restored_operations_are_unknown_and_never_auto_publish() {
    let first = Harness::new(Mode::Hold);
    let id = first.send("hello");
    flush().await;
    let signed = first.relay.posted()[0].clone();
    assert_eq!(
        first.journal.records()[0].signed.as_ref().map(|e| e.id),
        Some(id)
    );
    let second = first.reopen(Mode::Accept);
    second.outbox.ready().await.unwrap();
    flush().await;
    assert_eq!(second.pending_ids(), vec![id]);
    assert_eq!(second.delivery(&id), Delivery::Unknown);
    assert!(second.relay.posted().is_empty());
    second.outbox.retry(&id, None).unwrap();
    flush().await;
    assert_eq!(second.relay.posted(), vec![signed]);
}

#[tokio::test(start_paused = true)]
async fn a_publisher_that_never_answers_times_out_as_unknown_then_reconciles() {
    let h = Harness::with(
        Arc::new(Identity::ephemeral()),
        TestJournal::default(),
        FakeRelay::new(Mode::Hold),
        Duration::from_millis(100),
    );
    let id = h.send("hello");
    flush().await;
    assert_eq!(h.relay.posted().len(), 1);
    tokio::time::sleep(Duration::from_millis(101)).await;
    let op = h.op(&id);
    assert_eq!(op.delivery, Delivery::Unknown);
    assert_eq!(op.error.as_deref(), Some(TIMED_OUT));
    // An uncertain publish may have landed: confirmation reads start at once.
    let reads = h.relay.echo_reads();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].1, Priority::Foreground);
    h.relay.store(&h.relay.posted()[0]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(h.outbox.pending().is_empty());
    assert_eq!(h.delivery(&id), Delivery::Seen);
}

#[tokio::test(start_paused = true)]
async fn accepted_sends_poll_for_their_echo_on_schedule() {
    let h = Harness::new(Mode::Accept);
    h.outbox.ready().await.unwrap();
    let start = Instant::now();
    let id = h.send("hello");
    flush().await;
    assert_eq!(h.delivery(&id), Delivery::Accepted);
    tokio::time::sleep(Duration::from_secs(60)).await;
    let reads = h.relay.echo_reads();
    let offsets: Vec<u128> = reads
        .iter()
        .map(|(_, _, at)| (*at - start).as_millis())
        .collect();
    assert_eq!(offsets, [0, 500, 2_000, 6_000, 16_000]);
    let priorities: Vec<Priority> = reads.iter().map(|(_, priority, _)| *priority).collect();
    assert_eq!(
        priorities,
        [
            Priority::Foreground,
            Priority::Background,
            Priority::Background,
            Priority::Background,
            Priority::Background,
        ]
    );
    // Stays accepted with no further automatic work.
    assert_eq!(h.delivery(&id), Delivery::Accepted);
    assert_eq!(h.relay.posted().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn product_feedback_is_never_echo_confirmed() {
    let h = Harness::new(Mode::Accept);
    let id = h
        .outbox
        .send(
            Draft {
                kind: PRODUCT_FEEDBACK,
                content: "great app".into(),
                tags: Vec::new(),
            },
            SendOptions::default(),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(h.delivery(&id), Delivery::Accepted);
    assert!(h.relay.echo_reads().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_echo_before_the_ack_wins_over_a_late_failure() {
    let h = Harness::new(Mode::Hold);
    let id = h.send("hello");
    flush().await;
    let held = h.relay.take();
    h.outbox.observe(&[verified(&held.event)]);
    flush().await;
    // The attempt ended: its publication was dropped, not left hanging.
    assert!(held.reply.is_closed());
    held.lose();
    flush().await;
    assert!(h.outbox.pending().is_empty());
    let op = h.op(&id);
    assert_eq!(op.delivery, Delivery::Seen);
    assert!(op.error.is_none());
    assert_eq!(h.outbox.visible().len(), 1);
    // The confirmed record is committed too.
    assert_eq!(h.journal.records()[0].delivery, Delivery::Seen);
}

#[tokio::test(start_paused = true)]
async fn echoes_free_delivery_slots_even_without_acks() {
    let h = Harness::new(Mode::Hold);
    let ids: Vec<EventId> = (0..4).map(|i| h.send(&format!("queued {i}"))).collect();
    flush().await;
    let posted = h.relay.posted();
    assert_eq!(posted.len(), 3);
    let echoes: Vec<Verified> = posted.iter().map(verified).collect();
    h.outbox.observe(&echoes);
    flush().await;
    assert_eq!(h.relay.posted().len(), 4);
    assert_eq!(h.pending_ids(), vec![ids[3]]);
}

#[tokio::test(start_paused = true)]
async fn deadlines_count_from_enqueue_including_queue_wait() {
    let h = Harness::with(
        Arc::new(Identity::ephemeral()),
        TestJournal::default(),
        FakeRelay::new(Mode::Hold),
        Duration::from_millis(100),
    );
    for i in 0..3 {
        h.send(&format!("blocking {i}"));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let queued = h.send("waiting for a slot");
    flush().await;
    assert_eq!(h.relay.posted().len(), 3);
    // The blockers expire at 100 ms and free their slots; the queued send
    // starts then, but keeps its own deadline at 150 ms.
    tokio::time::sleep(Duration::from_millis(51)).await;
    assert_eq!(h.relay.posted().len(), 4);
    assert_eq!(h.delivery(&queued), Delivery::Sending);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.delivery(&queued), Delivery::Unknown);
    assert!(
        !h.outbox
            .pending()
            .iter()
            .any(|op| op.delivery == Delivery::Sending)
    );
}

#[tokio::test(start_paused = true)]
async fn a_queued_retry_deadline_preserves_unknown_without_dispatch() {
    let base = Harness::new(Mode::Hold);
    let prior = base.signed(kinds::CHAT, NOW, "prior send", vec![vec!["h", "c"]]);
    base.journal
        .set_records(vec![saved(&prior, Delivery::Unknown)]);
    base.outbox.dispose();
    let h = Harness::with(
        base.identity.clone(),
        base.journal.clone(),
        FakeRelay::new(Mode::Hold),
        Duration::from_millis(100),
    );
    h.outbox.ready().await.unwrap();
    for i in 0..3 {
        h.send(&format!("occupy {i}"));
    }
    h.outbox.retry(&prior.id, None).unwrap();
    flush().await;
    assert_eq!(h.relay.posted().len(), 3);
    tokio::time::sleep(Duration::from_millis(101)).await;
    assert_eq!(h.delivery(&prior.id), Delivery::Unknown);
    assert!(!h.relay.posted().iter().any(|event| event.id == prior.id));
}

#[tokio::test(start_paused = true)]
async fn failed_retries_keep_earlier_evidence_across_restarts() {
    for prior in [Delivery::Unknown, Delivery::Accepted] {
        let h = Harness::new(Mode::Hold);
        let id = h.send("hello");
        flush().await;
        let first = h.relay.take();
        if prior == Delivery::Unknown {
            first.lose();
        } else {
            first.accept();
        }
        flush().await;
        assert_eq!(h.delivery(&id), prior);
        h.outbox.retry(&id, None).unwrap();
        flush().await;
        h.relay.take().reject("restricted: membership changed");
        flush().await;
        let op = h.op(&id);
        assert_eq!(op.delivery, prior);
        assert_eq!(
            op.error.as_deref(),
            Some("Retry blocked: restricted: membership changed")
        );
        // An uncertain retry cannot downgrade an accepted send either.
        h.outbox.retry(&id, None).unwrap();
        flush().await;
        h.relay.take().lose();
        flush().await;
        let op = h.op(&id);
        assert_eq!(op.delivery, prior);
        assert_eq!(
            op.error.as_deref(),
            Some("Retry failed: Relay request could not be confirmed")
        );
        h.outbox.retry(&id, None).unwrap();
        flush().await;
        h.relay.take().reject("restricted: membership changed");
        flush().await;
        let restored = h.reopen(Mode::Hold);
        restored.outbox.ready().await.unwrap();
        let op = restored.op(&id);
        assert_eq!(op.delivery, Delivery::Unknown);
        assert_eq!(
            op.error.as_deref(),
            Some("Retry blocked: restricted: membership changed")
        );
    }
}

#[tokio::test(start_paused = true)]
async fn storage_failure_before_a_retry_dispatch_keeps_uncertainty() {
    let h = Harness::new(Mode::Hold);
    let id = h.send("hello");
    flush().await;
    h.relay.take().lose();
    flush().await;
    assert_eq!(h.delivery(&id), Delivery::Unknown);
    h.journal.fail(Some("disk full"));
    h.outbox.retry(&id, None).unwrap();
    flush().await;
    assert_eq!(h.relay.posted().len(), 1);
    let op = h.op(&id);
    assert_eq!(op.delivery, Delivery::Unknown);
    assert_eq!(op.error.as_deref(), Some("Retry failed: disk full"));
}

#[tokio::test(start_paused = true)]
async fn observation_batches_confirm_only_matching_pending_ids() {
    let base = Harness::new(Mode::Accept);
    let event = |content: &str| base.signed(kinds::CHAT, NOW, content, vec![vec!["h", "c"]]);
    let (first, second, pending, unrelated) = (
        event("first"),
        event("second"),
        event("pending"),
        event("unrelated"),
    );
    base.journal.set_records(
        [&first, &second, &pending]
            .into_iter()
            .map(|event| saved(event, Delivery::Unknown))
            .collect(),
    );
    let h = base.reopen(Mode::Accept);
    h.outbox.ready().await.unwrap();
    h.outbox
        .observe(&[verified(&unrelated), verified(&second), verified(&first)]);
    assert_eq!(h.pending_ids(), vec![pending.id]);
    let visible: Vec<(EventId, Delivery)> = h
        .outbox
        .visible()
        .iter()
        .map(|op| (op.id(), op.delivery))
        .collect();
    assert_eq!(
        visible,
        vec![
            (first.id, Delivery::Seen),
            (second.id, Delivery::Seen),
            (pending.id, Delivery::Unknown),
        ]
    );
    assert!(h.relay.posted().is_empty());
}

// ---------------------------------------------------------------------------
// Hydration

#[tokio::test(start_paused = true)]
async fn commits_after_hydration_never_drop_restored_intent() {
    let base = Harness::new(Mode::Hold);
    let restored = base.signed(kinds::CHAT, NOW, "older pending", vec![vec!["h", "c"]]);
    base.outbox.dispose();
    let journal = TestJournal::default();
    let release = journal.hold_load();
    let h = Harness::with(
        base.identity.clone(),
        journal.clone(),
        FakeRelay::new(Mode::Hold),
        Duration::from_millis(100),
    );
    let id = h.send("hello");
    tokio::time::sleep(Duration::from_millis(101)).await;
    assert_eq!(h.delivery(&id), Delivery::Failed);
    assert!(h.op(&id).signed.is_none());
    journal.set_records(vec![saved(&restored, Delivery::Unknown)]);
    release.send(()).unwrap();
    flush().await;
    let commits = journal.commits();
    assert!(!commits.is_empty());
    assert!(
        commits
            .iter()
            .all(|records| records.iter().any(|op| op.id() == restored.id))
    );
    assert!(h.relay.posted().is_empty());
    assert_eq!(h.pending_ids(), vec![restored.id, id]);
}

#[tokio::test(start_paused = true)]
async fn hydration_rejects_tampered_or_foreign_records() {
    let base = Harness::new(Mode::Accept);
    let event = base.signed(kinds::CHAT, NOW, "original", vec![vec!["h", "c"]]);
    let mut tampered = saved(&event, Delivery::Unknown);
    if let Some(signed) = &mut tampered.signed {
        signed.content = "changed".into();
    }
    let foreign = {
        let other = Harness::new(Mode::Accept);
        saved(
            &other.signed(kinds::CHAT, NOW, "theirs", vec![vec!["h", "c"]]),
            Delivery::Unknown,
        )
    };
    let mut bad_id = saved(&event, Delivery::Failed);
    bad_id.signed = None;
    bad_id.event.content = "edited".into();
    for record in [tampered, foreign, bad_id] {
        base.journal.set_records(vec![record]);
        let h = base.reopen(Mode::Accept);
        let error = h.outbox.ready().await.unwrap_err();
        assert_eq!(
            error,
            OutboxError::Storage("Could not load the outbox: Saved outbox event is invalid".into())
        );
        assert!(matches!(
            h.outbox.send(chat("x"), SendOptions::default()),
            Err(OutboxError::Storage(_))
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn hydration_maps_saved_states() {
    let base = Harness::new(Mode::Accept);
    let event = |content: &str| base.signed(kinds::CHAT, NOW, content, vec![vec!["h", "c"]]);
    let (sending, accepted, failed, seen) = (event("a"), event("b"), event("c"), event("d"));
    let mut unsigned_seen = saved(&event("e"), Delivery::Seen);
    unsigned_seen.signed = None;
    base.journal.set_records(vec![
        saved(&sending, Delivery::Sending),
        saved(&accepted, Delivery::Accepted),
        saved(&failed, Delivery::Failed),
        saved(&seen, Delivery::Seen),
        unsigned_seen.clone(),
    ]);
    let h = base.reopen(Mode::Accept);
    h.outbox.ready().await.unwrap();
    assert_eq!(h.delivery(&sending.id), Delivery::Unknown);
    assert_eq!(h.delivery(&accepted.id), Delivery::Unknown);
    assert_eq!(h.delivery(&failed.id), Delivery::Failed);
    assert_eq!(h.delivery(&seen.id), Delivery::Seen);
    assert_eq!(h.delivery(&unsigned_seen.id()), Delivery::Unknown);
    // Confirmed records are retained history, not pending work.
    assert!(!h.pending_ids().contains(&seen.id));
    flush().await;
    assert!(h.relay.posted().is_empty());
}

#[tokio::test(start_paused = true)]
async fn revocation_during_hydration_keeps_creation_receipts_and_all_pending() {
    let base = Harness::new(Mode::Accept);
    let channel = "11111111-1111-4111-8111-111111111111";
    let creation = base.signed(kinds::CREATE_CHANNEL, NOW, "", vec![vec!["h", channel]]);
    let message = base.signed(
        kinds::CHAT,
        NOW,
        "private content",
        vec![vec!["h", channel]],
    );
    let pending = base.signed(kinds::CHAT, NOW, "still pending", vec![vec!["h", channel]]);
    base.outbox.dispose();
    let journal = TestJournal::with(vec![
        saved(&creation, Delivery::Seen),
        saved(&message, Delivery::Seen),
        saved(&pending, Delivery::Unknown),
    ]);
    let release = journal.hold_load();
    let h = Harness::with(
        base.identity.clone(),
        journal,
        FakeRelay::new(Mode::Accept),
        DELIVERY_TIMEOUT,
    );
    h.outbox
        .purge_confirmed(|op| op.kind() == kinds::CREATE_CHANNEL);
    release.send(()).unwrap();
    h.outbox.ready().await.unwrap();
    assert_eq!(h.pending_ids(), vec![pending.id]);
    let visible: Vec<EventId> = h.outbox.visible().iter().map(|op| op.id()).collect();
    assert_eq!(visible, vec![creation.id, pending.id]);
    // A later purge drops readable history but never pending intent.
    h.outbox.purge_confirmed(|_| false);
    flush().await;
    let records: Vec<EventId> = h.journal.records().iter().map(|op| op.id()).collect();
    assert_eq!(records, vec![pending.id]);
    assert_eq!(h.op(&pending.id).signed.as_ref(), Some(&pending));
}

// ---------------------------------------------------------------------------
// Edits

#[tokio::test(start_paused = true)]
async fn edit_timestamps_increase_per_target_and_cap_clock_lead() {
    let base = Harness::new(Mode::Hold);
    let target = "a".repeat(64);
    let previous = base.signed(
        kinds::EDIT,
        NOW,
        "Previous",
        vec![vec!["h", "c"], vec!["e", &target]],
    );
    base.outbox.dispose();
    let journal = TestJournal::with(vec![saved(&previous, Delivery::Seen)]);
    let release = journal.hold_load();
    let h = Harness::with(
        base.identity.clone(),
        journal,
        FakeRelay::new(Mode::Hold),
        DELIVERY_TIMEOUT,
    );
    let edit = |content: &str, channel: &str, target: &str| Draft {
        kind: kinds::EDIT,
        content: content.into(),
        tags: vec![
            vec!["h".into(), channel.into()],
            vec!["e".into(), target.into()],
        ],
    };
    let refused = h
        .outbox
        .send(edit("Next", "c", &target), SendOptions::default())
        .unwrap_err();
    assert!(refused.to_string().contains("still loading"));
    release.send(()).unwrap();
    h.outbox.ready().await.unwrap();
    let send = |draft: Draft| h.outbox.send(draft, SendOptions::default()).unwrap();
    let first = send(edit("Next", "c", &target));
    let second = send(edit("Third", "c", &target));
    assert_eq!(h.op(&first).created_at(), NOW + 1);
    assert_eq!(h.op(&second).created_at(), NOW + 2);
    let other_target = send(edit("Next", "c", &"b".repeat(64)));
    let other_channel = send(edit("Next", "d", &target));
    assert_eq!(h.op(&other_target).created_at(), NOW);
    assert_eq!(h.op(&other_channel).created_at(), NOW);
    let before = h.outbox.pending();
    h.clock.0.store((NOW - 60) * 1000, Ordering::SeqCst);
    let error = h
        .outbox
        .send(edit("Too fast", "c", &target), SendOptions::default())
        .unwrap_err();
    assert!(error.to_string().contains("clock changed"));
    assert!(Arc::ptr_eq(&before, &h.outbox.pending()));
}

// ---------------------------------------------------------------------------
// Old events

#[tokio::test(start_paused = true)]
async fn old_events_are_looked_up_instead_of_reposted() {
    let base = Harness::new(Mode::Accept);
    let old = base.signed(kinds::CHAT, NOW - 16 * 60, "old", vec![vec!["h", "c"]]);
    let relay = FakeRelay::new(Mode::Accept);

    // Not found: a definitive refusal, never a re-dated resend.
    let result = publish_event(&relay, &old, NOW).await;
    assert_eq!(result, Err(PublishError::Rejected(TOO_OLD.into())));
    // Lookup failure: still uncertain.
    relay.state().lookup_error = Some("Relay is unreachable".into());
    let result = publish_event(&relay, &old, NOW).await;
    assert_eq!(
        result,
        Err(PublishError::Uncertain("Relay is unreachable".into()))
    );
    relay.state().lookup_error = None;
    relay.store(&old);
    assert_eq!(publish_event(&relay, &old, NOW).await, Ok(String::new()));
    assert!(relay.posted().is_empty());
    assert_eq!(relay.state().lookups, 3);
    // Within 15 minutes it is posted normally.
    let recent = base.signed(kinds::CHAT, NOW - 14 * 60, "recent", vec![vec!["h", "c"]]);
    assert_eq!(publish_event(&relay, &recent, NOW).await, Ok(String::new()));
    assert_eq!(relay.posted(), vec![recent]);

    // Through the outbox: a restored old send that landed becomes accepted.
    base.journal
        .set_records(vec![saved(&old, Delivery::Unknown)]);
    let h = base.reopen(Mode::Accept);
    h.relay.store(&old);
    h.outbox.ready().await.unwrap();
    h.outbox.retry(&old.id, None).unwrap();
    flush().await;
    assert!(h.relay.posted().is_empty());
    assert!(matches!(
        h.delivery(&old.id),
        Delivery::Accepted | Delivery::Seen
    ));
}

// ---------------------------------------------------------------------------
// Dismissal

#[tokio::test(start_paused = true)]
async fn dismissal_applies_only_after_its_commit() {
    let base = Harness::new(Mode::Accept);
    let creation = base.signed(
        kinds::CREATE_CHANNEL,
        NOW,
        "",
        vec![vec!["h", "11111111-1111-4111-8111-111111111111"]],
    );
    base.journal
        .set_records(vec![saved(&creation, Delivery::Seen)]);
    let h = base.reopen(Mode::Accept);
    h.outbox.ready().await.unwrap();
    assert!(h.outbox.pending().is_empty());
    h.journal.fail_once("journal unavailable");
    let error = h.outbox.dismiss(&creation.id).await.unwrap_err();
    assert_eq!(error, OutboxError::Storage("journal unavailable".into()));
    assert_eq!(h.outbox.visible().len(), 1);
    assert_eq!(h.journal.records().len(), 1);
    h.outbox.dismiss(&creation.id).await.unwrap();
    assert!(h.outbox.visible().is_empty());
    assert!(h.journal.records().is_empty());
}

#[tokio::test(start_paused = true)]
async fn dismissal_serializes_with_concurrent_intent() {
    #[derive(Clone, Copy, PartialEq)]
    enum Outcome {
        Commit,
        Reject,
        Echo,
    }
    for outcome in [Outcome::Commit, Outcome::Reject, Outcome::Echo] {
        let base = Harness::new(Mode::Accept);
        let edit = base.signed(
            kinds::EDIT,
            NOW,
            "Retained edit",
            vec![vec!["e", &"a".repeat(64)]],
        );
        base.journal
            .set_records(vec![saved(&edit, Delivery::Failed)]);
        let h = base.reopen(Mode::Accept);
        h.outbox.ready().await.unwrap();
        let gate = h.journal.hold_commit();
        let dismissal = tokio::spawn(h.outbox.dismiss(&edit.id));
        let duplicate = tokio::spawn(h.outbox.dismiss(&edit.id));
        flush().await;
        assert!(!dismissal.is_finished() && !duplicate.is_finished());
        assert_eq!(h.pending_ids(), vec![edit.id]);
        // A dismissal in flight blocks retry.
        h.outbox.retry(&edit.id, None).unwrap();
        assert_eq!(h.delivery(&edit.id), Delivery::Failed);
        let next = h.send("Concurrent intent");
        if outcome == Outcome::Echo {
            h.outbox.observe(&[verified(&edit)]);
        }
        gate.send(if outcome == Outcome::Commit {
            Ok(())
        } else {
            Err("Disk unavailable".into())
        })
        .unwrap();
        let result = dismissal.await.unwrap();
        assert_eq!(duplicate.await.unwrap(), result);
        assert_eq!(
            result,
            if outcome == Outcome::Commit {
                Ok(())
            } else {
                Err(OutboxError::Storage("Disk unavailable".into()))
            }
        );
        flush().await;
        assert_eq!(h.delivery(&next), Delivery::Accepted);
        let records = h.journal.records();
        assert_eq!(
            records
                .iter()
                .find(|op| op.id() == next)
                .map(|op| op.delivery),
            Some(Delivery::Accepted)
        );
        assert_eq!(h.relay.posted().len(), 1);
        let remaining = records.iter().find(|op| op.id() == edit.id);
        match outcome {
            Outcome::Commit => assert!(remaining.is_none()),
            Outcome::Reject => assert_eq!(remaining.map(|op| op.delivery), Some(Delivery::Failed)),
            Outcome::Echo => assert_eq!(remaining.map(|op| op.delivery), Some(Delivery::Seen)),
        }
        assert_eq!(
            h.pending_ids().contains(&edit.id),
            outcome == Outcome::Reject
        );
    }
}

// ---------------------------------------------------------------------------
// Guarded (invitation) operations

fn gate(active: &Arc<AtomicBool>) -> Gate {
    let active = active.clone();
    Arc::new(move || active.load(Ordering::SeqCst))
}

fn invitation() -> Draft {
    let member = "a".repeat(64);
    draft(kinds::PUT_USER, &[&["h", "c"], &["p", &member]])
}

#[tokio::test(start_paused = true)]
async fn guarded_operations_check_admission_before_signing_and_at_publisher_entry() {
    let h = Harness::new(Mode::Hold);
    let active = Arc::new(AtomicBool::new(true));
    let id = h
        .outbox
        .send(
            invitation(),
            SendOptions {
                recovery: None,
                gate: Some(gate(&active)),
            },
        )
        .unwrap();
    active.store(false, Ordering::SeqCst);
    flush().await;
    assert_eq!(h.delivery(&id), Delivery::Failed);
    assert!(h.op(&id).signed.is_none());
    assert_eq!(h.outbox.retry(&id, None), Err(OutboxError::Cancelled));

    // Signed, then cancelled while preparing: never published.
    active.store(true, Ordering::SeqCst);
    let release = h.relay.hold_prepare();
    h.outbox.retry(&id, Some(gate(&active))).unwrap();
    flush().await;
    assert!(h.op(&id).signed.is_some());
    active.store(false, Ordering::SeqCst);
    release.send(()).unwrap();
    flush().await;
    assert_eq!(h.delivery(&id), Delivery::Failed);
    assert!(h.relay.posted().is_empty());

    // Past publisher entry the result stands even if the gate closes.
    active.store(true, Ordering::SeqCst);
    h.outbox.retry(&id, Some(gate(&active))).unwrap();
    flush().await;
    assert_eq!(h.relay.posted().len(), 1);
    active.store(false, Ordering::SeqCst);
    h.relay.take().accept();
    flush().await;
    assert_eq!(h.delivery(&id), Delivery::Accepted);
}

#[tokio::test(start_paused = true)]
async fn guarded_operations_never_replay_without_live_admission() {
    let h = Harness::new(Mode::Accept);
    let active = Arc::new(AtomicBool::new(true));
    let id = h
        .outbox
        .send(
            invitation(),
            SendOptions {
                recovery: None,
                gate: Some(gate(&active)),
            },
        )
        .unwrap();
    active.store(false, Ordering::SeqCst);
    flush().await;
    assert_eq!(h.delivery(&id), Delivery::Failed);
    let restored = h.reopen(Mode::Accept);
    restored.outbox.ready().await.unwrap();
    assert!(restored.op(&id).guarded);
    assert_eq!(
        restored.outbox.retry(&id, None),
        Err(OutboxError::Cancelled)
    );
    flush().await;
    assert!(restored.relay.posted().is_empty());
    restored.outbox.retry(&id, Some(Arc::new(|| true))).unwrap();
    flush().await;
    assert_eq!(restored.relay.posted().len(), 1);
    assert_eq!(restored.delivery(&id), Delivery::Accepted);
}

#[tokio::test(start_paused = true)]
async fn a_closed_gate_refuses_the_send_itself() {
    let h = Harness::new(Mode::Accept);
    let closed: Gate = Arc::new(|| false);
    let result = h.outbox.send(
        invitation(),
        SendOptions {
            recovery: None,
            gate: Some(closed),
        },
    );
    assert_eq!(result, Err(OutboxError::Cancelled));
    assert!(h.outbox.pending().is_empty());
}

// ---------------------------------------------------------------------------
// Recovery

#[tokio::test(start_paused = true)]
async fn recovery_is_committed_with_intent_before_publication() {
    let h = Harness::new(Mode::Accept);
    h.outbox.ready().await.unwrap();
    let gate = h.journal.hold_commit();
    let id = h
        .outbox
        .send(chat("First message"), with_recovery())
        .unwrap();
    flush().await;
    assert!(h.relay.posted().is_empty());
    gate.send(Ok(())).unwrap();
    flush().await;
    let first = &h.journal.commits()[0][0];
    assert_eq!(first.id(), id);
    assert_eq!(first.recovery, Some(recovery()));
    assert_eq!(h.relay.posted().len(), 1);

    let failing = Harness::new(Mode::Accept);
    failing.outbox.ready().await.unwrap();
    failing.journal.fail(Some("Disk full"));
    let id = failing
        .outbox
        .send(chat("First message"), with_recovery())
        .unwrap();
    flush().await;
    assert_eq!(failing.delivery(&id), Delivery::Failed);
    assert!(failing.relay.posted().is_empty());
}

#[tokio::test(start_paused = true)]
async fn recovery_keys_wait_for_hydration_stay_unique_and_retry_exactly() {
    let first = Harness::new(Mode::Lose);
    first.outbox.ready().await.unwrap();
    let id = first
        .outbox
        .send(chat("First message"), with_recovery())
        .unwrap();
    flush().await;
    assert_eq!(first.delivery(&id), Delivery::Unknown);
    assert_eq!(first.journal.records()[0].delivery, Delivery::Unknown);
    let published = first.relay.posted()[0].clone();
    first.outbox.dispose();

    let release = first.journal.hold_load();
    let second = Harness::with(
        first.identity.clone(),
        first.journal.clone(),
        FakeRelay::new(Mode::Accept),
        DELIVERY_TIMEOUT,
    );
    let refused = OutboxError::Refused(RECOVER_FIRST.into());
    assert_eq!(
        second.outbox.send(chat("again"), with_recovery()),
        Err(refused.clone())
    );
    release.send(()).unwrap();
    second.outbox.ready().await.unwrap();
    assert_eq!(
        second.outbox.send(chat("again"), with_recovery()),
        Err(refused)
    );
    let error = second.outbox.dismiss(&id).await.unwrap_err();
    assert!(error.to_string().contains("Confirm this message"));
    second.outbox.retry(&id, None).unwrap();
    flush().await;
    assert_eq!(second.relay.posted(), vec![published]);
}

#[tokio::test(start_paused = true)]
async fn failures_can_be_dismissed_but_confirmed_recovery_waits_for_acknowledgement() {
    let h = Harness::new(Mode::Reject("blocked: not sent"));
    h.outbox.ready().await.unwrap();
    let id = h
        .outbox
        .send(chat("First message"), with_recovery())
        .unwrap();
    flush().await;
    assert_eq!(h.delivery(&id), Delivery::Failed);
    h.outbox.dismiss(&id).await.unwrap();
    assert!(h.journal.records().is_empty());

    h.relay.set_mode(Mode::Accept);
    let next = h
        .outbox
        .send(chat("First message"), with_recovery())
        .unwrap();
    flush().await;
    assert_eq!(h.delivery(&next), Delivery::Accepted);
    assert!(h.outbox.dismiss(&next).await.is_err());
    let event = h.relay.posted()[1].clone();
    h.outbox.observe(&[verified(&event)]);
    let op = h.op(&next);
    assert_eq!(
        (op.delivery, op.recovery.clone()),
        (Delivery::Seen, Some(recovery()))
    );
    assert_eq!(h.pending_ids(), vec![next]);
    h.outbox.acknowledge(&next).await.unwrap();
    assert!(h.outbox.pending().is_empty());
    let saved = &h.journal.records()[0];
    assert!(saved.recovery.is_none() && saved.acknowledged);
}

#[tokio::test(start_paused = true)]
async fn acknowledgement_failure_keeps_recovery_and_latest_evidence() {
    let h = Harness::new(Mode::Accept);
    h.outbox.ready().await.unwrap();
    let id = h
        .outbox
        .send(chat("First message"), with_recovery())
        .unwrap();
    flush().await;
    assert_eq!(h.journal.records()[0].delivery, Delivery::Accepted);
    let gate = h.journal.hold_commit();
    let outbox = h.outbox.clone();
    let acknowledged = tokio::spawn(async move { outbox.acknowledge(&id).await });
    flush().await;
    let event = h.relay.posted()[0].clone();
    h.outbox.observe(&[verified(&event)]);
    assert_eq!(
        h.outbox.send(chat("again"), with_recovery()),
        Err(OutboxError::Refused(RECOVER_FIRST.into()))
    );
    gate.send(Err("Disk full".into())).unwrap();
    assert_eq!(
        acknowledged.await.unwrap(),
        Err(OutboxError::Storage("Disk full".into()))
    );
    let op = h.op(&id);
    assert_eq!(
        (op.delivery, op.recovery.clone()),
        (Delivery::Seen, Some(recovery()))
    );
    h.outbox.acknowledge(&id).await.unwrap();
    assert!(h.journal.records()[0].recovery.is_none());
    assert!(h.outbox.send(chat("again"), with_recovery()).is_ok());
}

/// The app calls in from GPUI threads, which have no Tokio context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_and_acknowledge_work_off_the_runtime() {
    let h = Harness::new(Mode::Accept);
    h.outbox.ready().await.unwrap();
    let id = h.send("First message");
    while h.delivery(&id) != Delivery::Accepted {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let outbox = h.outbox.clone();
    std::thread::spawn(move || {
        futures::executor::block_on(async {
            outbox.recover(&id, recovery()).await.unwrap();
            outbox.acknowledge(&id).await.unwrap();
        })
    })
    .join()
    .unwrap();
    let saved = &h.journal.records()[0];
    assert!(saved.recovery.is_none() && saved.acknowledged);
}

#[tokio::test(start_paused = true)]
async fn recover_adopts_a_confirmed_echo_durably() {
    let h = Harness::new(Mode::Accept);
    h.outbox.ready().await.unwrap();
    let id = h.send("First message");
    flush().await;
    let event = h.relay.posted()[0].clone();
    h.outbox.observe(&[verified(&event)]);
    assert!(h.outbox.pending().is_empty());
    h.outbox.recover(&id, recovery()).await.unwrap();
    let record = &h.journal.records()[0];
    assert_eq!(
        (record.delivery, record.recovery.clone()),
        (Delivery::Seen, Some(recovery()))
    );
    assert!(h.outbox.dismiss(&id).await.is_err());
    // Different recovery for the same operation, or the same key elsewhere.
    let other = Recovery {
        value: "other".into(),
        ..recovery()
    };
    assert!(h.outbox.recover(&id, other).await.is_err());
    let second = h.send("Second");
    assert!(h.outbox.recover(&second, recovery()).await.is_err());
    let missing = EventId::from_hex(&"0".repeat(64)).unwrap();
    let error = h.outbox.recover(&missing, recovery()).await.unwrap_err();
    assert!(error.to_string().contains("unavailable"));
    flush().await;

    let restored = h.reopen(Mode::Accept);
    restored.outbox.ready().await.unwrap();
    assert_eq!(restored.op(&id).recovery, Some(recovery()));
    assert!(restored.pending_ids().contains(&id));
    restored.outbox.acknowledge(&id).await.unwrap();
    assert!(!restored.pending_ids().contains(&id));
    let completed = restored.reopen(Mode::Accept);
    completed.outbox.ready().await.unwrap();
    assert!(!completed.pending_ids().contains(&id));
    assert!(completed.op(&id).acknowledged);
}

#[tokio::test(start_paused = true)]
async fn member_additions_retire_on_acknowledgement_without_retry() {
    let h = Harness::new(Mode::Lose);
    h.outbox.ready().await.unwrap();
    let id = h
        .outbox
        .send(
            invitation(),
            SendOptions {
                recovery: Some(Recovery {
                    key: "member-add:c:person".into(),
                    value: "1".into(),
                }),
                gate: None,
            },
        )
        .unwrap();
    flush().await;
    assert_eq!(h.journal.records()[0].delivery, Delivery::Unknown);
    h.outbox.acknowledge(&id).await.unwrap();
    let op = h.op(&id);
    assert!(op.acknowledged);
    assert_eq!(op.delivery, Delivery::Unknown);
    assert!(h.outbox.pending().is_empty());
    h.outbox.retry(&id, None).unwrap();
    flush().await;
    assert_eq!(h.relay.posted().len(), 1);
    let restored = h.reopen(Mode::Accept);
    restored.outbox.ready().await.unwrap();
    restored.outbox.retry(&id, None).unwrap();
    flush().await;
    assert!(restored.outbox.pending().is_empty());
    assert!(restored.op(&id).acknowledged);
    assert!(restored.relay.posted().is_empty());
}

// ---------------------------------------------------------------------------
// Workflow commands

fn workflow_delete(h: &Harness) -> Draft {
    let coordinate = format!("30620:{}:workflow", h.identity.pubkey_hex());
    draft(kinds::DELETION, &[&["h", "c"], &["a", &coordinate]])
}

#[tokio::test(start_paused = true)]
async fn workflow_commands_await_their_receipt_after_an_echo() {
    let h = Harness::new(Mode::Hold);
    let id = h
        .outbox
        .send(workflow_delete(&h), SendOptions::default())
        .unwrap();
    flush().await;
    let held = h.relay.take();
    h.outbox.observe(&[verified(&held.event)]);
    // Still pending: the receipt is what the caller waits for.
    assert_eq!(h.pending_ids(), vec![id]);
    assert_eq!(h.delivery(&id), Delivery::Seen);
    let body = receipt(&id, true, "response:{\"ok\":true}");
    held.respond(Ok(body));
    flush().await;
    assert_eq!(
        h.receipts.lock().unwrap().clone(),
        vec![(id, Some("response:{\"ok\":true}".to_owned()))]
    );
    assert!(h.outbox.pending().is_empty());
    assert_eq!(h.delivery(&id), Delivery::Seen);
}

#[tokio::test(start_paused = true)]
async fn workflow_commands_with_a_lost_receipt_are_never_retried() {
    let h = Harness::new(Mode::Hold);
    let id = h
        .outbox
        .send(workflow_delete(&h), SendOptions::default())
        .unwrap();
    flush().await;
    h.relay.take().lose();
    flush().await;
    let op = h.op(&id);
    assert_eq!(op.delivery, Delivery::Unknown);
    assert_eq!(op.error.as_deref(), Some(WORKFLOW_UNCERTAIN));
    assert_eq!(h.receipts.lock().unwrap().clone(), vec![(id, None)]);
    h.outbox.retry(&id, None).unwrap();
    flush().await;
    assert_eq!(h.relay.posted().len(), 1);
    // Inspection and dismissal still work.
    h.outbox.dismiss(&id).await.unwrap();
    assert!(h.outbox.get(&id).is_none());
}

// ---------------------------------------------------------------------------
// Publish response mapping (§3.7)

#[test]
fn publish_responses_map_to_outcomes() {
    use PublishError::{Rejected, Uncertain};
    let id = EventId::from_hex(&"0".repeat(64)).unwrap();
    let other = EventId::from_hex(&"1".repeat(64)).unwrap();
    let ok = |body: Value| Ok(serde_json::to_vec(&body).unwrap());
    let status = |status: u16, message: &str| {
        Err(HttpError::Status {
            status,
            message: message.into(),
            retry_after: None,
            not_sent: false,
        })
    };
    let hex = id.to_hex();
    type Case = (Result<Vec<u8>, HttpError>, Result<String, PublishError>);
    let cases: Vec<Case> = vec![
        (
            ok(json!({"event_id": hex, "accepted": true, "message": ""})),
            Ok(String::new()),
        ),
        (
            ok(json!({"event_id": hex, "accepted": true, "message": "duplicate:"})),
            Ok("duplicate:".into()),
        ),
        (
            ok(json!({"event_id": hex, "accepted": true})),
            Ok(String::new()),
        ),
        // A landed duplicate reaction counts as accepted (§9.3 #3).
        (
            ok(
                json!({"event_id": hex, "accepted": false, "message": "duplicate: reaction already exists"}),
            ),
            Ok("duplicate: reaction already exists".into()),
        ),
        (
            ok(json!({"event_id": hex, "accepted": false, "message": "blocked: no"})),
            Err(Rejected("blocked: no".into())),
        ),
        (
            ok(json!({"event_id": hex, "accepted": false})),
            Err(Rejected("Relay rejected the message".into())),
        ),
        (
            ok(json!({"event_id": other.to_hex(), "accepted": true})),
            Err(Uncertain(INVALID_RECEIPT.into())),
        ),
        (
            ok(json!({"event_id": hex, "accepted": "yes"})),
            Err(Uncertain(INVALID_RECEIPT.into())),
        ),
        (
            Ok(b"not json".to_vec()),
            Err(Uncertain(INVALID_RECEIPT.into())),
        ),
        (Ok(vec![0xff, 0xfe]), Err(Uncertain(INVALID_RECEIPT.into()))),
        (
            Ok(vec![b' '; MAX_RECEIPT_BYTES + 1]),
            Err(Uncertain(
                "Relay delivery receipt exceeds the size limit".into(),
            )),
        ),
        (
            status(409, "conflict"),
            Err(Rejected(
                "conflict: the relay state changed; reload before writing again".into(),
            )),
        ),
        (
            status(429, "rate-limited: quota exceeded; retry in 12s"),
            Err(Rejected(
                "rate-limited: quota exceeded; retry in 12s".into(),
            )),
        ),
        (
            status(429, "rate-limited: quota exceededly"),
            Err(Uncertain(
                "Relay delivery could not be confirmed (429)".into(),
            )),
        ),
        (
            status(503, "unavailable"),
            Err(Uncertain(
                "Relay delivery could not be confirmed (503)".into(),
            )),
        ),
        (
            status(500, "internal server error"),
            Err(Uncertain(
                "Relay delivery could not be confirmed (500)".into(),
            )),
        ),
        (
            Err(HttpError::Transport("reset".into())),
            Err(Uncertain("Relay request could not be confirmed".into())),
        ),
        (
            Err(HttpError::TooLarge),
            Err(Uncertain("Relay request could not be confirmed".into())),
        ),
        (
            Err(HttpError::Paused {
                retry_after: Duration::from_secs(5),
            }),
            Err(Rejected("Relay requests paused; retry in 5s".into())),
        ),
        (
            Err(HttpError::Capacity),
            Err(Rejected("Relay API queue capacity reached".into())),
        ),
        // An API quota refusal without a hint is a minute.
        (
            status(429, "rate-limited: quota exceeded"),
            Err(Rejected(
                "rate-limited: quota exceeded; retry in 60s".into(),
            )),
        ),
        // The relay's own `sent: false` proves non-delivery; only known
        // reasons are kept.
        (
            Err(HttpError::Status {
                status: 503,
                message: "rate-limited: shared admission unavailable".into(),
                retry_after: None,
                not_sent: true,
            }),
            Err(Rejected(
                "rate-limited: shared admission unavailable".into(),
            )),
        ),
        (
            Err(HttpError::Status {
                status: 503,
                message: "upstream detail".into(),
                retry_after: None,
                not_sent: true,
            }),
            Err(Rejected("Relay request failed (503)".into())),
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(accept_publish(input, &id), expected);
    }
    for code in [400, 401, 403, 404, 413, 422] {
        assert_eq!(
            accept_publish(status(code, "x"), &id),
            Err(Rejected(format!("Relay rejected the message ({code})")))
        );
    }
}

#[test]
fn workflow_operations_are_recognized() {
    let identity = Identity::ephemeral();
    let template = |kind: u16, tags: Vec<Vec<String>>| {
        identity
            .template(Purpose::Content, kind, 1, tags, String::new())
            .unwrap()
    };
    let a = |value: &str| vec![vec!["a".to_owned(), value.to_owned()]];
    assert!(is_workflow_operation(&template(5, a("30620:pk:wf"))));
    assert!(is_workflow_operation(&template(5, a("+0030620:pk:wf"))));
    assert!(!is_workflow_operation(&template(5, a("30621:pk:wf"))));
    assert!(!is_workflow_operation(&template(5, Vec::new())));
    assert!(!is_workflow_operation(&template(9, a("30620:pk:wf"))));
}

#[test]
fn operations_round_trip_through_json() {
    let identity = Identity::ephemeral();
    let unsigned = identity
        .template(
            Purpose::Content,
            9,
            1,
            vec![vec!["h".into(), "c".into()]],
            "hi".into(),
        )
        .unwrap();
    let signed = identity.sign(Purpose::Content, unsigned.clone()).unwrap();
    let op = Operation {
        event: unsigned,
        signed: Some(signed),
        recovery: Some(recovery()),
        acknowledged: false,
        guarded: true,
        delivery: Delivery::Unknown,
        error: Some("Retry failed: x".into()),
    };
    let json = serde_json::to_value(&op).unwrap();
    assert_eq!(json["delivery"], "unknown");
    assert!(json.get("acknowledged").is_none());
    assert_eq!(serde_json::from_value::<Operation>(json).unwrap(), op);
}

// ---------------------------------------------------------------------------
// Send observers (`observeSend`)

#[tokio::test(start_paused = true)]
async fn send_observers_see_fresh_sends_and_run_after_delivery() {
    let h = Harness::new(Mode::Hold);
    h.outbox.ready().await.unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let delivered: Arc<Mutex<Vec<(String, usize)>>> = Arc::default();
    let (log, done) = (seen.clone(), delivered.clone());
    let observation = h
        .outbox
        .observe_send(Arc::new(move |event: &UnsignedEvent| {
            log.lock().unwrap().push(event.content.clone());
            let done = done.clone();
            let content = event.content.clone();
            (content != "ignored").then(|| -> DeliveryWork {
                Box::new(move |pending: &[UnsignedEvent]| {
                    done.lock().unwrap().push((content, pending.len()));
                })
            })
        }));
    // An observer that panics never touches the send or the others.
    let _broken = h
        .outbox
        .observe_send(Arc::new(|_: &UnsignedEvent| panic!("observer bug")));
    let first = h.send("first");
    h.send("second");
    h.send("ignored");
    assert_eq!(*seen.lock().unwrap(), ["first", "second", "ignored"]);
    flush().await;
    assert!(
        delivered.lock().unwrap().is_empty(),
        "nothing delivered yet"
    );

    // Accepted: the work runs with the other observed send still waiting.
    h.relay.take().accept();
    flush().await;
    assert_eq!(*delivered.lock().unwrap(), [("first".to_string(), 1)]);
    assert_eq!(h.delivery(&first), Delivery::Accepted);

    // An echo is delivery too.
    let held = h.relay.take();
    h.outbox.observe(&[verified(&held.event)]);
    flush().await;
    assert_eq!(delivered.lock().unwrap().len(), 2);

    // Dropping the handle stops observing.
    drop(observation);
    h.send("after");
    assert_eq!(seen.lock().unwrap().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn dismissed_sends_drop_their_delivery_work() {
    let h = Harness::new(Mode::Hold);
    h.outbox.ready().await.unwrap();
    let waiting: Arc<Mutex<Vec<usize>>> = Arc::default();
    let log = waiting.clone();
    let _observation = h.outbox.observe_send(Arc::new(move |_: &UnsignedEvent| {
        let log = log.clone();
        Some(Box::new(move |pending: &[UnsignedEvent]| {
            log.lock().unwrap().push(pending.len());
        }) as DeliveryWork)
    }));
    let rejected = h.send("rejected");
    flush().await;
    h.relay.take().reject("blocked: not allowed");
    flush().await;
    h.outbox.dismiss(&rejected).await.unwrap();
    h.send("next");
    flush().await;
    h.relay.take().accept();
    flush().await;
    // The dismissed send no longer counts as waiting.
    assert_eq!(*waiting.lock().unwrap(), [0]);
}
