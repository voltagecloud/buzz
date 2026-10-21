//! Finite, verified reads for one community session (`reader.ts`).
//!
//! - Identical concurrent reads share one request. Each caller is a
//!   consumer; dropping one consumer's future never cancels another, and
//!   the request stops when the last consumer leaves.
//! - At most 3 requests run, at most 1 of them background. Foreground work
//!   goes first; otherwise first in, first out.
//! - Every read has a deadline counted from when it was queued.
//! - Nothing is retried here. Callers decide.

use crate::event::{Verified, Verifier};
use crate::http::{HttpError, RelayHttp};
use crate::socket::Filter;
use futures::future::BoxFuture;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

pub const MAX_FILTERS: usize = 4;
pub const MAX_LIMIT: u64 = 500;
const MAX_KEY_BYTES: usize = 64 * 1024;
/// Verified responses larger than this are refused.
pub const MAX_RESPONSE_BYTES: usize = 8 << 20;

pub use crate::http::Priority;

#[derive(Clone, Copy, Debug)]
pub struct ReadOptions {
    pub priority: Priority,
    /// Never join a read already in flight: a write preflight must observe
    /// state after its own intent.
    pub fresh: bool,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            priority: Priority::Foreground,
            fresh: false,
        }
    }
}

impl ReadOptions {
    pub fn background() -> Self {
        Self {
            priority: Priority::Background,
            fresh: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadErrorKind {
    /// 401/403: the viewer may not read this.
    Denied,
    Unavailable,
    InvalidResponse,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ReadError {
    pub kind: ReadErrorKind,
    pub message: String,
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
}

impl ReadError {
    fn new(kind: ReadErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retry_after: None,
        }
    }

    fn from_http(error: HttpError) -> Self {
        match error {
            HttpError::Status {
                status,
                message,
                retry_after,
                ..
            } => Self {
                kind: if matches!(status, 401 | 403) {
                    ReadErrorKind::Denied
                } else {
                    ReadErrorKind::Unavailable
                },
                // Only quota guidance is worth showing verbatim.
                message: if message.starts_with("rate-limited: quota exceeded") {
                    message
                } else {
                    format!("Relay request failed ({status})")
                },
                status: Some(status),
                retry_after,
            },
            HttpError::Paused { retry_after } => Self {
                kind: ReadErrorKind::Unavailable,
                message: HttpError::Paused { retry_after }.to_string(),
                status: Some(429),
                retry_after: Some(retry_after),
            },
            HttpError::Capacity => Self::new(
                ReadErrorKind::Unavailable,
                "Relay API queue capacity reached",
            ),
            HttpError::RequestTooLarge => {
                Self::new(ReadErrorKind::InvalidResponse, "Relay read is too large")
            }
            HttpError::TooLarge | HttpError::Invalid(_) => Self::new(
                ReadErrorKind::InvalidResponse,
                "Relay returned an invalid response",
            ),
            HttpError::Transport(_) => {
                Self::new(ReadErrorKind::Unavailable, "Relay is unreachable")
            }
        }
    }
}

pub type Events = Arc<[Verified]>;

/// Validates and canonicalizes filters: sorted keys (serde_json's map is
/// ordered), sorted and deduplicated `kinds`, `authors`, `ids` and tag
/// values. The canonical form is both the coalescing key and what is sent.
pub fn canonical(filters: Vec<Filter>) -> Result<(Vec<Filter>, String), ReadError> {
    let invalid = |message: &str| ReadError::new(ReadErrorKind::InvalidResponse, message);
    if filters.is_empty() || filters.len() > MAX_FILTERS {
        return Err(invalid("A relay read needs 1 to 4 filters"));
    }
    let mut canonical = Vec::with_capacity(filters.len());
    for filter in filters {
        // Sorted by key whatever the map's order: with serde_json's
        // `preserve_order` (which any crate in the build may enable),
        // field order would otherwise split equal reads across keys.
        let mut entries: Vec<_> = filter.into_iter().filter(|(_, v)| !v.is_null()).collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut filter: Filter = entries.into_iter().collect();
        for (key, value) in filter.iter_mut() {
            if (key == "kinds" || key == "authors" || key == "ids" || key.starts_with('#'))
                && let Value::Array(items) = value
            {
                items.sort_by(compare_json);
                items.dedup();
            }
        }
        let kinds_ok = filter
            .get("kinds")
            .and_then(Value::as_array)
            .is_some_and(|kinds| {
                !kinds.is_empty() && kinds.iter().all(|k| k.as_u64().is_some_and(|k| k <= 65535))
            });
        let ids_only = !filter.contains_key("kinds")
            && filter
                .get("ids")
                .and_then(Value::as_array)
                .is_some_and(|ids| !ids.is_empty());
        if !kinds_ok && !ids_only {
            return Err(invalid("Each relay read filter needs kinds or ids"));
        }
        if let Some(limit) = filter.get("limit")
            && !limit
                .as_u64()
                .is_some_and(|limit| (1..=MAX_LIMIT).contains(&limit))
        {
            return Err(invalid("Relay read limits must be 1 to 500"));
        }
        canonical.push(filter);
    }
    let key = serde_json::to_string(&canonical).expect("filters serialize");
    if key.len() > MAX_KEY_BYTES {
        return Err(invalid("Relay read is too large"));
    }
    Ok((canonical, key))
}

fn compare_json(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        _ => a.to_string().cmp(&b.to_string()),
    }
}

type Predicate = Box<dyn Fn(&[Filter]) -> bool + Send>;

/// Where reads go. [`RelayHttp`] in the app; fakes in tests.
pub trait Source: Send + Sync + 'static {
    fn query(
        &self,
        filters: Vec<Filter>,
        priority: Priority,
    ) -> BoxFuture<'static, Result<Vec<Value>, HttpError>>;
}

impl Source for RelayHttp {
    fn query(
        &self,
        filters: Vec<Filter>,
        priority: Priority,
    ) -> BoxFuture<'static, Result<Vec<Value>, HttpError>> {
        let http = self.clone();
        Box::pin(async move { http.query_with(&filters, priority).await })
    }
}

enum Command {
    Read {
        filters: Vec<Filter>,
        key: String,
        priority: Priority,
        consumer: u64,
        reply: oneshot::Sender<Result<Events, ReadError>>,
    },
    Cancel {
        key: String,
        consumer: u64,
    },
    Finished {
        key: String,
        generation: u64,
        result: Result<Events, ReadError>,
    },
    Invalidate(Predicate),
    Promote(Predicate),
}

#[derive(Clone)]
pub struct Reader {
    commands: mpsc::UnboundedSender<Command>,
    next: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Clone, Debug)]
pub struct ReaderOptions {
    pub deadline: Duration,
    pub max_pending: usize,
    pub slots: usize,
}

impl Default for ReaderOptions {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(10),
            max_pending: 128,
            slots: 3,
        }
    }
}

impl Reader {
    pub fn spawn(
        runtime: &tokio::runtime::Handle,
        source: Arc<dyn Source>,
        verifier: Arc<Verifier>,
        options: ReaderOptions,
    ) -> Self {
        let (commands, queue) = mpsc::unbounded_channel();
        runtime.spawn(
            Actor {
                source,
                verifier,
                options,
                jobs: HashMap::new(),
                order: VecDeque::new(),
                generation: 0,
                commands: commands.clone(),
            }
            .run(queue),
        );
        Self {
            commands,
            next: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    pub async fn read(
        &self,
        filters: Vec<Filter>,
        options: ReadOptions,
    ) -> Result<Events, ReadError> {
        let (filters, mut key) = canonical(filters)?;
        let consumer = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if options.fresh {
            key.push_str(&format!(":fresh:{consumer}"));
        }
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Read {
                filters,
                key: key.clone(),
                priority: options.priority,
                consumer,
                reply,
            })
            .map_err(|_| ReadError::new(ReadErrorKind::Cancelled, "Relay session closed"))?;
        let mut guard = CancelOnDrop {
            commands: &self.commands,
            key: Some(key),
            consumer,
        };
        let result = answer.await.unwrap_or_else(|_| {
            Err(ReadError::new(
                ReadErrorKind::Cancelled,
                "Relay read cancelled",
            ))
        });
        guard.key = None;
        result
    }

    /// Cancels queued and running reads whose filters match.
    pub fn invalidate(&self, predicate: impl Fn(&[Filter]) -> bool + Send + 'static) {
        let _ = self.commands.send(Command::Invalidate(Box::new(predicate)));
    }

    /// Moves matching queued reads to the foreground without changing their
    /// deadlines.
    pub fn promote(&self, predicate: impl Fn(&[Filter]) -> bool + Send + 'static) {
        let _ = self.commands.send(Command::Promote(Box::new(predicate)));
    }
}

struct CancelOnDrop<'a> {
    commands: &'a mpsc::UnboundedSender<Command>,
    key: Option<String>,
    consumer: u64,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let _ = self.commands.send(Command::Cancel {
                key,
                consumer: self.consumer,
            });
        }
    }
}

struct Job {
    filters: Vec<Filter>,
    priority: Priority,
    consumers: HashMap<u64, oneshot::Sender<Result<Events, ReadError>>>,
    deadline: Instant,
    /// Set while running; distinguishes this run's completion from a
    /// replaced job's.
    running: Option<(u64, tokio::task::AbortHandle)>,
}

struct Actor {
    source: Arc<dyn Source>,
    verifier: Arc<Verifier>,
    options: ReaderOptions,
    jobs: HashMap<String, Job>,
    order: VecDeque<String>,
    generation: u64,
    commands: mpsc::UnboundedSender<Command>,
}

impl Actor {
    async fn run(mut self, mut queue: mpsc::UnboundedReceiver<Command>) {
        loop {
            let next_deadline = self.jobs.values().map(|job| job.deadline).min();
            tokio::select! {
                command = queue.recv() => match command {
                    Some(command) => self.handle(command),
                    None => return,
                },
                _ = sleep_until(next_deadline) => self.expire(),
            }
            self.pump();
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Read {
                filters,
                key,
                priority,
                consumer,
                reply,
            } => {
                if let Some(job) = self.jobs.get_mut(&key) {
                    job.consumers.insert(consumer, reply);
                    if priority == Priority::Foreground {
                        job.priority = Priority::Foreground;
                    }
                    return;
                }
                if self.jobs.len() >= self.options.max_pending {
                    let _ = reply.send(Err(ReadError::new(
                        ReadErrorKind::Unavailable,
                        "Too many pending relay reads",
                    )));
                    return;
                }
                self.jobs.insert(
                    key.clone(),
                    Job {
                        filters,
                        priority,
                        consumers: HashMap::from([(consumer, reply)]),
                        deadline: Instant::now() + self.options.deadline,
                        running: None,
                    },
                );
                self.order.push_back(key);
            }
            Command::Cancel { key, consumer } => {
                if let Some(job) = self.jobs.get_mut(&key) {
                    job.consumers.remove(&consumer);
                    if job.consumers.is_empty() {
                        self.remove(&key);
                    }
                }
            }
            Command::Finished {
                key,
                generation,
                result,
            } => {
                let current = self
                    .jobs
                    .get(&key)
                    .and_then(|job| job.running.as_ref())
                    .is_some_and(|(running, _)| *running == generation);
                if current && let Some(job) = self.remove(&key) {
                    for (_, consumer) in job.consumers {
                        let _ = consumer.send(result.clone());
                    }
                }
            }
            Command::Invalidate(predicate) => {
                let keys: Vec<String> = self
                    .jobs
                    .iter()
                    .filter(|(_, job)| predicate(&job.filters))
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in keys {
                    self.fail(
                        &key,
                        ReadError::new(ReadErrorKind::Cancelled, "Relay read cancelled"),
                    );
                }
            }
            Command::Promote(predicate) => {
                for job in self.jobs.values_mut() {
                    if job.running.is_none() && predicate(&job.filters) {
                        job.priority = Priority::Foreground;
                    }
                }
            }
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.fail(
                &key,
                ReadError::new(ReadErrorKind::Unavailable, "Relay read timed out"),
            );
        }
    }

    fn fail(&mut self, key: &str, error: ReadError) {
        if let Some(job) = self.remove(key) {
            for (_, consumer) in job.consumers {
                let _ = consumer.send(Err(error.clone()));
            }
        }
    }

    fn remove(&mut self, key: &str) -> Option<Job> {
        let job = self.jobs.remove(key)?;
        self.order.retain(|queued| queued != key);
        if let Some((_, handle)) = &job.running {
            handle.abort();
        }
        Some(job)
    }

    fn pump(&mut self) {
        loop {
            let running: Vec<&Job> = self
                .jobs
                .values()
                .filter(|job| job.running.is_some())
                .collect();
            if running.len() >= self.options.slots {
                return;
            }
            let background_running = running
                .iter()
                .any(|job| job.priority == Priority::Background);
            let queued = |priority: Option<Priority>| {
                self.order.iter().find(|key| {
                    let job = &self.jobs[*key];
                    job.running.is_none() && priority.is_none_or(|p| job.priority == p)
                })
            };
            let next = queued(Some(Priority::Foreground))
                .or_else(|| (!background_running).then(|| queued(None)).flatten())
                .cloned();
            let Some(key) = next else {
                return;
            };
            self.generation += 1;
            let generation = self.generation;
            let job = self.jobs.get_mut(&key).expect("queued job exists");
            let filters = job.filters.clone();
            let priority = job.priority;
            let source = self.source.clone();
            let verifier = self.verifier.clone();
            let commands = self.commands.clone();
            let task_key = key.clone();
            let task = tokio::spawn(async move {
                let result = fetch(source, verifier, filters, priority).await;
                let _ = commands.send(Command::Finished {
                    key: task_key,
                    generation,
                    result,
                });
            });
            job.running = Some((generation, task.abort_handle()));
        }
    }
}

async fn fetch(
    source: Arc<dyn Source>,
    verifier: Arc<Verifier>,
    filters: Vec<Filter>,
    priority: Priority,
) -> Result<Events, ReadError> {
    let raw = source
        .query(filters, priority)
        .await
        .map_err(ReadError::from_http)?;
    // Verification is CPU work; keep it off the I/O threads.
    let events = tokio::task::spawn_blocking(move || verifier.verify_all(&raw))
        .await
        .map_err(|_| ReadError::new(ReadErrorKind::Cancelled, "Relay read cancelled"))?
        .map_err(|error| ReadError::new(ReadErrorKind::InvalidResponse, error.to_string()))?;
    let bytes: usize = events.iter().map(Verified::byte_size).sum();
    if bytes > MAX_RESPONSE_BYTES {
        return Err(ReadError::new(
            ReadErrorKind::InvalidResponse,
            "Relay response exceeded its size limit",
        ));
    }
    Ok(events.into())
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => futures::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn filter(value: Value) -> Filter {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn canonical_form_sorts_and_dedupes() {
        let (a, key_a) = canonical(vec![filter(
            json!({"limit": 5, "kinds": [9, 7, 9], "#h": ["b", "a"]}),
        )])
        .unwrap();
        let (_, key_b) = canonical(vec![filter(
            json!({"#h": ["a", "b"], "kinds": [7, 9], "limit": 5}),
        )])
        .unwrap();
        assert_eq!(key_a, key_b);
        assert_eq!(a[0]["kinds"], json!([7, 9]));
    }

    #[test]
    fn rejects_unbounded_or_malformed_reads() {
        for bad in [
            vec![],
            vec![filter(json!({"limit": 5}))],
            vec![filter(json!({"kinds": [], "limit": 5}))],
            vec![filter(json!({"kinds": [9], "limit": 501}))],
            vec![filter(json!({"kinds": [9], "limit": 0}))],
            vec![filter(json!({"kinds": [70000]}))],
            (0..5).map(|_| filter(json!({"kinds": [9]}))).collect(),
        ] {
            assert!(canonical(bad.clone()).is_err(), "{bad:?}");
        }
        assert!(canonical(vec![filter(json!({"ids": ["ab"]}))]).is_ok());
    }
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;
    use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind};
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use tokio::sync::Semaphore;

    /// Holds every query until the test releases it, counting calls and
    /// peak concurrency. Tests decide when fetches finish instead of racing
    /// timers, so they stay deterministic on a loaded machine.
    struct Fake {
        gate: Arc<Semaphore>,
        calls: Mutex<Vec<Vec<Filter>>>,
        running: Arc<AtomicUsize>,
        peak: AtomicUsize,
        event: Value,
    }

    impl Fake {
        fn new(open: bool) -> Arc<Self> {
            let event = EventBuilder::new(Kind::from_u16(9), "hi")
                .finalize(&Keys::generate())
                .unwrap();
            Arc::new(Self {
                gate: Arc::new(Semaphore::new(if open {
                    Semaphore::MAX_PERMITS
                } else {
                    0
                })),
                calls: Mutex::default(),
                running: Default::default(),
                peak: Default::default(),
                event: serde_json::to_value(event).unwrap(),
            })
        }

        fn release(&self, count: usize) {
            self.gate.add_permits(count);
        }

        fn calls(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        /// Waits until `count` queries have started.
        async fn started(&self, count: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.calls() < count {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("queries did not start");
        }
    }

    impl Source for Fake {
        fn query(
            &self,
            filters: Vec<Filter>,
            _priority: Priority,
        ) -> BoxFuture<'static, Result<Vec<Value>, HttpError>> {
            self.calls.lock().unwrap().push(filters);
            let now = self.running.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            let (gate, running, event) =
                (self.gate.clone(), self.running.clone(), self.event.clone());
            Box::pin(async move {
                gate.acquire().await.expect("gate stays open").forget();
                running.fetch_sub(1, SeqCst);
                Ok(vec![event])
            })
        }
    }

    fn spawn_reader(fake: &Arc<Fake>, options: ReaderOptions) -> Reader {
        Reader::spawn(
            &tokio::runtime::Handle::current(),
            fake.clone(),
            Arc::new(Verifier::default()),
            options,
        )
    }

    fn filter(n: u64) -> Vec<Filter> {
        vec![
            json!({"kinds": [9], "limit": n})
                .as_object()
                .unwrap()
                .clone(),
        ]
    }

    #[tokio::test]
    async fn identical_reads_share_one_request() {
        let fake = Fake::new(true);
        let reader = spawn_reader(&fake, ReaderOptions::default());
        let (a, b) = tokio::join!(
            reader.read(filter(5), ReadOptions::default()),
            reader.read(filter(5), ReadOptions::default())
        );
        assert_eq!(a.unwrap().len(), 1);
        assert_eq!(b.unwrap().len(), 1);
        assert_eq!(fake.calls(), 1);

        let fresh = ReadOptions {
            fresh: true,
            ..Default::default()
        };
        let (_, _) = tokio::join!(reader.read(filter(5), fresh), reader.read(filter(5), fresh));
        assert_eq!(fake.calls(), 3, "fresh reads never join");
    }

    #[tokio::test]
    async fn dropping_one_consumer_keeps_the_other() {
        let fake = Fake::new(false);
        let reader = spawn_reader(&fake, ReaderOptions::default());
        let kept = tokio::spawn({
            let reader = reader.clone();
            async move { reader.read(filter(5), ReadOptions::default()).await }
        });
        let dropped = tokio::spawn({
            let reader = reader.clone();
            async move { reader.read(filter(5), ReadOptions::default()).await }
        });
        fake.started(1).await;
        dropped.abort();
        let _ = dropped.await;
        fake.release(1);
        assert!(kept.await.unwrap().is_ok());
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn runs_at_most_three_and_one_background() {
        let fake = Fake::new(false);
        let reader = spawn_reader(&fake, ReaderOptions::default());
        let reads: Vec<_> = (1..=6)
            .map(|n| {
                let reader = reader.clone();
                tokio::spawn(async move { reader.read(filter(n), ReadOptions::default()).await })
            })
            .collect();
        fake.started(3).await;
        // Give the reader every chance to over-admit before checking.
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(fake.calls(), 3);
        fake.release(6);
        for read in reads {
            assert!(read.await.unwrap().is_ok());
        }
        assert_eq!(fake.peak.load(SeqCst), 3);

        let fake = Fake::new(false);
        let reader = spawn_reader(&fake, ReaderOptions::default());
        let reads: Vec<_> = (1..=3)
            .map(|n| {
                let reader = reader.clone();
                tokio::spawn(async move { reader.read(filter(n), ReadOptions::background()).await })
            })
            .collect();
        fake.started(1).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(fake.calls(), 1, "only one background read at a time");
        fake.release(3);
        for read in reads {
            assert!(read.await.unwrap().is_ok());
        }
        assert_eq!(fake.peak.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn deadline_counts_queue_time() {
        let fake = Fake::new(false);
        let reader = spawn_reader(
            &fake,
            ReaderOptions {
                deadline: Duration::from_millis(200),
                slots: 1,
                ..Default::default()
            },
        );
        let (first, second) = tokio::join!(
            reader.read(filter(1), ReadOptions::default()),
            reader.read(filter(2), ReadOptions::default())
        );
        assert_eq!(first.unwrap_err().message, "Relay read timed out");
        // The second read never left the queue, yet its deadline still ran.
        assert_eq!(second.unwrap_err().message, "Relay read timed out");
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn invalidate_cancels_matching_reads() {
        let fake = Fake::new(false);
        let reader = spawn_reader(&fake, ReaderOptions::default());
        let read = tokio::spawn({
            let reader = reader.clone();
            async move { reader.read(filter(7), ReadOptions::default()).await }
        });
        fake.started(1).await;
        reader.invalidate(|filters| filters[0]["limit"] == json!(7));
        assert_eq!(
            read.await.unwrap().unwrap_err().kind,
            ReadErrorKind::Cancelled
        );
    }
}
