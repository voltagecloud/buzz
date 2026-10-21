//! Finite reads and writes over HTTPS, authenticated with NIP-98.

use crate::origin::RelayOrigin;
use crate::socket::Filter;
use buzz_identity::Identity;
use nostr::prelude::Event;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

/// Larger responses are refused rather than buffered (`relay.rs`).
pub const MAX_RESPONSE: usize = 16 << 20;
/// Larger request bodies are refused before anything is signed or sent.
pub const MAX_REQUEST: usize = 1 << 20;
/// Requests running at once per relay and identity.
const LANE_SLOTS: usize = 6;
/// Requests waiting for a slot before new ones are refused.
const LANE_QUEUE: usize = 128;
/// Pause assumed when a quota refusal gives no `retry in Ns` hint.
const DEFAULT_QUOTA_SECS: u64 = 60;
const MAX_QUOTA_SECS: u64 = 86_400;
/// One presence snapshot read at most this often, outside the slots.
const PRESENCE_GATE: Duration = Duration::from_secs(5);

/// Whose request goes first when the lane is full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Priority {
    #[default]
    Foreground,
    Background,
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("could not reach the relay: {0}")]
    Transport(String),
    /// The relay answered with an error status.
    #[error("relay answered {status}: {message}")]
    Status {
        status: u16,
        message: String,
        /// From `Retry-After`, when the relay is rate limiting.
        retry_after: Option<Duration>,
        /// The relay said it did not act on the request (`sent: false`).
        not_sent: bool,
    },
    #[error("relay response is too large")]
    TooLarge,
    #[error("relay response is invalid: {0}")]
    Invalid(String),
    /// Not sent: the relay asked us to back off.
    #[error("Relay requests paused; retry in {}s", .retry_after.as_secs_f64().ceil())]
    Paused { retry_after: Duration },
    /// Not sent: too many requests already wait for this relay.
    #[error("Relay API queue capacity reached")]
    Capacity,
    /// Not sent: the body is over [`MAX_REQUEST`].
    #[error("Invalid relay request body")]
    RequestTooLarge,
}

impl HttpError {
    /// Whether the request certainly never reached the relay.
    pub fn not_sent(&self) -> bool {
        matches!(
            self,
            Self::Paused { .. } | Self::Capacity | Self::RequestTooLarge
        ) || matches!(self, Self::Status { not_sent: true, .. })
    }
}

#[derive(Clone)]
pub struct RelayHttp {
    client: reqwest::Client,
    origin: RelayOrigin,
    identity: Arc<Identity>,
    lane: Arc<Lane>,
}

/// Admission for every signed request to one relay from one identity
/// (`http-admission.ts`): at most 6 running, foreground before background,
/// at most 128 waiting. A relay quota refusal pauses the whole lane and
/// fails everything queued at once instead of piling onto a relay that
/// already said no.
struct Lane {
    state: Mutex<LaneState>,
}

#[derive(Default)]
struct LaneState {
    paused_until: Option<Instant>,
    active: usize,
    next_ticket: u64,
    queue: VecDeque<Ticket>,
    presence_busy: bool,
    presence_next: Option<Instant>,
}

struct Ticket {
    id: u64,
    priority: Priority,
    start: oneshot::Sender<Result<(), HttpError>>,
}

impl LaneState {
    fn paused(&self) -> Option<Duration> {
        let until = self.paused_until?;
        let now = Instant::now();
        (until > now).then(|| until - now)
    }

    /// Starts queued requests that fit (`pump`).
    fn pump(&mut self) {
        if let Some(retry_after) = self.paused() {
            for ticket in self.queue.drain(..) {
                let _ = ticket.start.send(Err(HttpError::Paused { retry_after }));
            }
            return;
        }
        while self.active < LANE_SLOTS && !self.queue.is_empty() {
            let index = self
                .queue
                .iter()
                .position(|ticket| ticket.priority == Priority::Foreground)
                .unwrap_or(0);
            let ticket = self.queue.remove(index).expect("queued ticket");
            if ticket.start.send(Ok(())).is_ok() {
                self.active += 1;
            }
        }
    }
}

/// A running request's slot; freeing it starts the next one.
struct Slot(Arc<Lane>);

impl Drop for Slot {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().expect("lane lock");
        state.active -= 1;
        state.pump();
    }
}

/// A presence read's lease, outside the six slots.
struct PresenceLease(Arc<Lane>);

impl Drop for PresenceLease {
    fn drop(&mut self) {
        self.0.state.lock().expect("lane lock").presence_busy = false;
    }
}

/// Removes a request from the queue if its caller gives up waiting, and
/// frees the slot if it was granted after the caller's last poll.
struct Waiting<'a> {
    lane: &'a Lane,
    id: u64,
    started: Option<oneshot::Receiver<Result<(), HttpError>>>,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(mut started) = self.started.take() {
            let mut state = self.lane.state.lock().expect("lane lock");
            state.queue.retain(|ticket| ticket.id != self.id);
            if let Ok(Ok(())) = started.try_recv() {
                state.active -= 1;
                state.pump();
            }
        }
    }
}

impl Lane {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(LaneState::default()),
        })
    }

    async fn admit(self: &Arc<Self>, priority: Priority) -> Result<Slot, HttpError> {
        let (id, started) = {
            let mut state = self.state.lock().expect("lane lock");
            if let Some(retry_after) = state.paused() {
                return Err(HttpError::Paused { retry_after });
            }
            if state.queue.len() >= LANE_QUEUE {
                return Err(HttpError::Capacity);
            }
            if state.active < LANE_SLOTS && state.queue.is_empty() {
                state.active += 1;
                return Ok(Slot(self.clone()));
            }
            let (start, started) = oneshot::channel();
            state.next_ticket += 1;
            let id = state.next_ticket;
            state.queue.push_back(Ticket {
                id,
                priority,
                start,
            });
            state.pump();
            (id, started)
        };
        let mut waiting = Waiting {
            lane: self,
            id,
            started: Some(started),
        };
        let started = waiting.started.as_mut().expect("waiting receiver").await;
        waiting.started = None;
        match started {
            Ok(Ok(())) => Ok(Slot(self.clone())),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(HttpError::Capacity),
        }
    }

    fn pause(&self, seconds: u64) {
        let until = Instant::now() + Duration::from_secs(seconds.min(MAX_QUOTA_SECS) + 1);
        let mut state = self.state.lock().expect("lane lock");
        state.paused_until = Some(state.paused_until.map_or(until, |at| at.max(until)));
        state.pump();
    }

    /// The presence lease, or how long until one may be tried.
    fn try_presence(self: &Arc<Self>) -> Result<PresenceLease, Duration> {
        let mut state = self.state.lock().expect("lane lock");
        let now = Instant::now();
        let gate = [state.paused_until, state.presence_next]
            .into_iter()
            .flatten()
            .max()
            .filter(|at| *at > now);
        if let Some(at) = gate {
            return Err(at - now);
        }
        if state.presence_busy {
            return Err(Duration::ZERO);
        }
        state.presence_busy = true;
        state.presence_next = Some(now + PRESENCE_GATE);
        Ok(PresenceLease(self.clone()))
    }
}

impl RelayHttp {
    pub fn new(origin: RelayOrigin, identity: Arc<Identity>) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("build the HTTP client");
        Self {
            client,
            origin,
            identity,
            lane: Lane::new(),
        }
    }

    pub fn identity(&self) -> &Arc<Identity> {
        &self.identity
    }

    pub fn origin(&self) -> &RelayOrigin {
        &self.origin
    }

    /// `POST /query`. Events with bad signatures are dropped.
    pub async fn query(&self, filters: &[Filter]) -> Result<Vec<Event>, HttpError> {
        Ok(self
            .query_raw(filters)
            .await?
            .into_iter()
            .filter_map(|value| serde_json::from_value::<Event>(value).ok())
            .filter(|event| event.verify().is_ok())
            .collect())
    }

    /// `POST /query` returning the raw event array, for callers that verify
    /// it themselves.
    pub async fn query_raw(&self, filters: &[Filter]) -> Result<Vec<serde_json::Value>, HttpError> {
        self.query_with(filters, Priority::Foreground).await
    }

    pub async fn query_with(
        &self,
        filters: &[Filter],
        priority: Priority,
    ) -> Result<Vec<serde_json::Value>, HttpError> {
        let body = serde_json::to_string(filters).expect("filters serialize");
        let bytes = self.post_with("/query", body, priority).await?;
        serde_json::from_slice(&bytes).map_err(|error| HttpError::Invalid(error.to_string()))
    }

    /// A signed `POST` with a JSON body. Never retried here: a write that
    /// failed in transport may still have reached the relay.
    pub async fn post(&self, path: &str, body: String) -> Result<Vec<u8>, HttpError> {
        self.post_with(path, body, Priority::Foreground).await
    }

    pub async fn post_with(
        &self,
        path: &str,
        body: String,
        priority: Priority,
    ) -> Result<Vec<u8>, HttpError> {
        if body.len() > MAX_REQUEST {
            return Err(HttpError::RequestTooLarge);
        }
        let _slot = self.lane.admit(priority).await?;
        self.send(path, body).await
    }

    /// A presence snapshot read on its lossy lease: at most one at a time
    /// and one per five seconds, outside the ordinary slots. `Paused` when
    /// the lease is not available yet.
    pub async fn post_presence(&self, body: String) -> Result<Vec<u8>, HttpError> {
        if body.len() > MAX_REQUEST {
            return Err(HttpError::RequestTooLarge);
        }
        let _lease = self
            .lane
            .try_presence()
            .map_err(|retry_after| HttpError::Paused { retry_after })?;
        self.send("/query", body).await
    }

    async fn send(&self, path: &str, body: String) -> Result<Vec<u8>, HttpError> {
        let url = self.origin.endpoint(path);
        let auth = self
            .identity
            .http_auth(&url, "POST", Some(body.as_bytes()))
            .map_err(|error| HttpError::Invalid(error.to_string()))?;
        // Request timings for profiling (`profiling.ts`): time to headers,
        // to the whole body, and the relay's own Server-Timing stages.
        let started = Instant::now();
        let sent = body.len();
        let response = self
            .client
            .post(&url)
            .header("Authorization", auth)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| HttpError::Transport(error.to_string()))?;
        let headers = started.elapsed();
        let status = response.status().as_u16();
        let server_timing = response
            .headers()
            .get("server-timing")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let result = read(response).await;
        tracing::debug!(
            target: "buzz_relay::profile",
            path,
            status,
            sent,
            received = result.as_ref().map_or(0, Vec::len),
            headers_ms = headers.as_millis() as u64,
            total_ms = started.elapsed().as_millis() as u64,
            server_timing = server_timing.as_deref().unwrap_or(""),
            "relay request"
        );
        if let Err(HttpError::Status {
            status: 429,
            message,
            ..
        }) = &result
            && let Some(seconds) = api_quota(message)
        {
            self.lane.pause(seconds);
        }
        result
    }

    /// A NIP-98 signed `GET` of a fixed route, such as workflow run
    /// history. Bodies over `limit` bytes are refused.
    pub async fn get(&self, path: &str, limit: usize) -> Result<Vec<u8>, HttpError> {
        let _slot = self.lane.admit(Priority::Foreground).await?;
        let url = self.origin.endpoint(path);
        let auth = self
            .identity
            .http_auth(&url, "GET", None)
            .map_err(|error| HttpError::Invalid(error.to_string()))?;
        let response = self
            .client
            .get(&url)
            .header("Authorization", auth)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|error| HttpError::Transport(error.to_string()))?;
        let result = read(response).await;
        if let Err(HttpError::Status {
            status: 429,
            message,
            ..
        }) = &result
            && let Some(seconds) = api_quota(message)
        {
            self.lane.pause(seconds);
        }
        let bytes = result?;
        if bytes.len() > limit {
            return Err(HttpError::TooLarge);
        }
        Ok(bytes)
    }

    /// `GET /` as NIP-11 relay information.
    pub async fn information(&self) -> Result<serde_json::Value, HttpError> {
        self.get_public("/").await
    }

    /// An unauthenticated `GET` of a JSON document, such as NIP-11 or the
    /// join policy. The relay only answers JSON on `/` with this `Accept`.
    pub async fn get_public(&self, path: &str) -> Result<serde_json::Value, HttpError> {
        let response = self
            .client
            .get(self.origin.endpoint(path))
            .header("Accept", "application/nostr+json")
            .send()
            .await
            .map_err(|error| HttpError::Transport(error.to_string()))?;
        let bytes = read(response).await?;
        serde_json::from_slice(&bytes).map_err(|error| HttpError::Invalid(error.to_string()))
    }
}

async fn read(mut response: reqwest::Response) -> Result<Vec<u8>, HttpError> {
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_RESPONSE)
    {
        return Err(HttpError::TooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| HttpError::Transport(error.to_string()))?
    {
        if bytes.len() + chunk.len() > MAX_RESPONSE {
            return Err(HttpError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let body = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        let message = body
            .as_ref()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|e| e.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).chars().take(200).collect());
        let not_sent = body
            .as_ref()
            .is_some_and(|value| value.get("sent") == Some(&serde_json::Value::Bool(false)));
        return Err(HttpError::Status {
            status: status.as_u16(),
            message,
            retry_after,
            not_sent,
        });
    }
    Ok(bytes)
}

/// A relay API quota refusal (`apiFailure`): `rate-limited: quota
/// exceeded`, optionally `; retry in Ns`, pauses the lane N (default 60,
/// at most 86 400) seconds plus one.
pub(crate) fn api_quota(message: &str) -> Option<u64> {
    let rest = message.strip_prefix("rate-limited: quota exceeded")?;
    if rest.is_empty() {
        return Some(DEFAULT_QUOTA_SECS);
    }
    if !rest.starts_with(';') {
        return None;
    }
    let seconds = rest
        .strip_prefix("; retry in ")
        .and_then(|n| n.strip_suffix('s'))
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map_or(DEFAULT_QUOTA_SECS, |n| n.parse().unwrap_or(MAX_QUOTA_SECS));
    Some(seconds.min(MAX_QUOTA_SECS))
}

/// The text kept from a failed relay API response (`apiFailure`): a few
/// known refusals verbatim, a publication refusal by category, otherwise
/// only the status.
pub(crate) fn api_failure_text(status: u16, message: &str) -> String {
    const KNOWN: &[&str] = &[
        "Query concurrency limit",
        "Live stream capacity reached",
        "Relay unreachable",
    ];
    if message.starts_with("rate-limited:") {
        crate::socket::publication_refusal(message)
    } else if KNOWN.contains(&message) {
        message.to_string()
    } else {
        format!("Relay request failed ({status})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_text_keeps_only_known_reasons() {
        assert_eq!(
            api_failure_text(503, "Query concurrency limit"),
            "Query concurrency limit"
        );
        assert_eq!(
            api_failure_text(503, "secret upstream text"),
            "Relay request failed (503)"
        );
        assert_eq!(
            api_failure_text(503, "rate-limited: shared admission unavailable"),
            "rate-limited: shared admission unavailable"
        );
        assert_eq!(
            api_failure_text(503, "rate-limited: something new"),
            "rate-limited: unrecognized reason"
        );
    }

    #[test]
    fn quota_refusals_pause_by_their_hint() {
        assert_eq!(
            api_quota("rate-limited: quota exceeded; retry in 12s"),
            Some(12)
        );
        assert_eq!(api_quota("rate-limited: quota exceeded"), Some(60));
        assert_eq!(
            api_quota("rate-limited: quota exceeded; retry in 999999s"),
            Some(86_400)
        );
        assert_eq!(api_quota("rate-limited: quota exceeded; soon"), Some(60));
        assert_eq!(api_quota("rate-limited: quota exceededly"), None);
        assert_eq!(api_quota("Query concurrency limit"), None);
    }

    #[tokio::test]
    async fn the_lane_runs_six_foreground_first() {
        let lane = Lane::new();
        let mut slots = Vec::new();
        for _ in 0..LANE_SLOTS {
            slots.push(lane.admit(Priority::Background).await.unwrap());
        }
        let order = Arc::new(Mutex::new(Vec::new()));
        let waiter = |priority: Priority, name: &'static str| {
            let (lane, order) = (lane.clone(), order.clone());
            tokio::spawn(async move {
                let slot = lane.admit(priority).await.unwrap();
                order.lock().unwrap().push(name);
                slot
            })
        };
        let background = waiter(Priority::Background, "background");
        while lane.state.lock().unwrap().queue.is_empty() {
            tokio::task::yield_now().await;
        }
        let foreground = waiter(Priority::Foreground, "foreground");
        while lane.state.lock().unwrap().queue.len() < 2 {
            tokio::task::yield_now().await;
        }
        // The waiting foreground request takes the first free slot.
        drop(slots.pop());
        let held = foreground.await.unwrap();
        drop(slots.pop());
        let _ = background.await.unwrap();
        drop(held);
        assert_eq!(*order.lock().unwrap(), ["foreground", "background"]);
    }

    #[tokio::test]
    async fn a_waiter_dropped_after_its_grant_frees_the_slot() {
        let lane = Lane::new();
        let mut slots = Vec::new();
        for _ in 0..LANE_SLOTS {
            slots.push(lane.admit(Priority::Foreground).await.unwrap());
        }
        let mut waiter = Box::pin(lane.admit(Priority::Foreground));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        // The freed slot goes to the waiter, which never polls again.
        drop(slots.pop());
        assert!(lane.state.lock().unwrap().queue.is_empty());
        drop(waiter);
        assert_eq!(lane.state.lock().unwrap().active, LANE_SLOTS - 1);
    }

    #[tokio::test]
    async fn the_queue_is_bounded_and_a_pause_fails_it() {
        let lane = Lane::new();
        let mut slots = Vec::new();
        for _ in 0..LANE_SLOTS {
            slots.push(lane.admit(Priority::Foreground).await.unwrap());
        }
        let mut waiting = Vec::new();
        for _ in 0..LANE_QUEUE {
            let lane = lane.clone();
            waiting.push(tokio::spawn(async move {
                lane.admit(Priority::Foreground).await.map(drop)
            }));
        }
        while lane.state.lock().unwrap().queue.len() < LANE_QUEUE {
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            lane.admit(Priority::Foreground).await,
            Err(HttpError::Capacity)
        ));
        // A quota pause fails everything queued at once.
        lane.pause(5);
        for waiting in waiting {
            assert!(matches!(
                waiting.await.unwrap(),
                Err(HttpError::Paused { .. })
            ));
        }
        assert!(matches!(
            lane.admit(Priority::Foreground).await,
            Err(HttpError::Paused { .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn presence_leases_are_lossy_gated_and_outside_the_slots() {
        let lane = Lane::new();
        let mut slots = Vec::new();
        for _ in 0..LANE_SLOTS {
            slots.push(lane.admit(Priority::Foreground).await.unwrap());
        }
        let lease = lane.try_presence().unwrap();
        assert_eq!(lane.try_presence().err(), Some(PRESENCE_GATE));
        drop(lease);
        tokio::time::advance(PRESENCE_GATE).await;
        let lease = lane.try_presence().unwrap();
        tokio::time::advance(PRESENCE_GATE).await;
        assert_eq!(
            lane.try_presence().err(),
            Some(Duration::ZERO),
            "one at a time"
        );
        drop(lease);
        lane.pause(10);
        assert_eq!(lane.try_presence().err(), Some(Duration::from_secs(11)));
    }
}
