//! Durable ACP session discovery and replay. The live channel subscription
//! overlaps every history query; replayed ids suppress its queued duplicates.

use super::*;
use nostr::PublicKey;

#[path = "discovery.rs"]
mod discovery;
use discovery::history_list;

#[path = "replay.rs"]
mod replay;
#[path = "transcripts.rs"]
mod transcripts;

pub(super) struct Replay {
    pub events: Vec<Event>,
    pub labels: HashMap<PublicKey, String>,
    pub turns: Vec<transcripts::ReplayTurn>,
}

const LIST_PAGE: usize = 20;
const QUERY_PAGE: usize = 100;
const MAX_SCAN: usize = 1000;
const MAX_REPLAY_BYTES: usize = 4 * 1024 * 1024;
const MAX_HISTORY_OPS: usize = 4;
const HISTORY_TIMEOUT: Duration = Duration::from_secs(30);

fn problem(message: &str) -> CliError {
    CliError::Other(message.to_owned())
}

fn cursor(event: &Event) -> String {
    format!("{}:{}", event.created_at.as_secs(), event.id.to_hex())
}

fn parse_cursor(raw: &str) -> Result<(u64, EventId), CliError> {
    let (time, id) = raw
        .split_once(':')
        .ok_or_else(|| problem("invalid session cursor"))?;
    let time: u64 = time
        .parse()
        .map_err(|_| problem("invalid session cursor"))?;
    let id = EventId::from_hex(id).map_err(|_| problem("invalid session cursor"))?;
    Ok((time, id))
}

fn apply_cursor(filter: &mut Value, raw: &str) -> Result<(), CliError> {
    let (time, id) = parse_cursor(raw)?;
    filter["until"] = json!(time);
    filter["before_id"] = json!(id.to_hex());
    Ok(())
}

fn has_tag(event: &Event, name: &str, value: &str) -> bool {
    event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.first().map(String::as_str) == Some(name)
            && parts.get(1).map(String::as_str) == Some(value)
    })
}

fn scoped(scope: &Scope, event: &Event) -> bool {
    event.kind.as_u16() == 9
        && event.verify().is_ok()
        && has_tag(event, "h", &scope.channel.to_string())
}

fn root_for(scope: &Scope, event: &Event) -> bool {
    scoped(scope, event)
        && buzz_core::nip10::parse_thread_markers(&event.tags)
            .resolve()
            .is_none()
}

fn involves(scope: &Scope, event: &Event) -> bool {
    event.pubkey == scope.agent || has_tag(event, "p", &scope.agent.to_hex())
}

fn authors(scope: &Scope) -> Vec<PublicKey> {
    let mut authors = vec![scope.owner];
    if scope.owner != scope.signer.public_key() {
        authors.push(scope.signer.public_key());
    }
    authors
}

fn mention_filter(scope: &Scope) -> Value {
    json!({"kinds": [9], "#h": [scope.channel.to_string()], "#p": [scope.agent.to_hex()], "limit": QUERY_PAGE, "consistency":"strong"})
}

fn alias_filter(scope: &Scope) -> Value {
    let mut filter = mention_filter(scope);
    filter["authors"] = json!(authors(scope)
        .iter()
        .map(PublicKey::to_hex)
        .collect::<Vec<_>>());
    filter
}

fn newest_first(events: &mut [Event]) {
    events.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
}

impl<R: BridgeRelay> Bridge<R> {
    pub(super) async fn on_history_list(
        &mut self,
        id: Value,
        params: &Value,
    ) -> Result<(), CliError> {
        if self.history_ops >= MAX_HISTORY_OPS {
            return self
                .respond_error(id, wire::INTERNAL_ERROR, "history is busy; retry")
                .await;
        }
        self.history_ops += 1;
        let (relay, scope, params, ops) = (
            Arc::clone(&self.relay),
            self.scope.clone(),
            params.clone(),
            self.ops.clone(),
        );
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                HISTORY_TIMEOUT,
                history_list(relay.as_ref(), &scope, &params),
            )
            .await
            .unwrap_or_else(|_| Err(problem("session listing timed out; retry")));
            let _ = ops.send(Op::HistoryListed { id, result }).await;
        });
        Ok(())
    }

    pub(super) async fn finish_history_list(
        &self,
        id: Value,
        result: Result<Value, CliError>,
    ) -> Result<(), CliError> {
        match result {
            Ok(page) => self.send(wire::ok(id, page)).await,
            Err(error) => {
                self.respond_error(id, wire::INTERNAL_ERROR, &error.to_string())
                    .await
            }
        }
    }

    pub(super) async fn on_history_load(
        &mut self,
        id: Value,
        params: &Value,
    ) -> Result<(), CliError> {
        let Some(raw) = params.get("sessionId").and_then(Value::as_str) else {
            return self
                .respond_error(id, wire::INVALID_PARAMS, "sessionId is required")
                .await;
        };
        if EventId::from_hex(raw).is_err() && uuid::Uuid::parse_str(raw).is_err() {
            return self
                .respond_error(
                    id,
                    wire::INVALID_PARAMS,
                    "sessionId must be a thread event id or persisted ACP session id",
                )
                .await;
        }
        if self.sessions.get(raw).is_some_and(|s| s.pending.is_some()) {
            return self
                .respond_error(
                    id,
                    wire::INVALID_REQUEST,
                    "a prompt is running in this session",
                )
                .await;
        }
        if self.history_ops >= MAX_HISTORY_OPS {
            return self
                .respond_error(id, wire::INTERNAL_ERROR, "history is busy; retry")
                .await;
        }
        let raw = raw.to_owned();
        let token = self.next_tick();
        let prior = self
            .sessions
            .get(&raw)
            .map(|session| (session.generation, session.last_used));
        self.loading.insert(
            raw.clone(),
            HistoryLoad {
                token,
                overflowed: false,
                project: params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .and_then(|cwd| projects::project_of_cwd(&self.scope.home, cwd)),
            },
        );
        self.history_ops += 1;
        let (relay, scope, ops, names) = (
            Arc::clone(&self.relay),
            self.scope.clone(),
            self.ops.clone(),
            Arc::clone(&self.names),
        );
        tokio::spawn(async move {
            let replay = async {
                let events = load_events(relay.as_ref(), &scope, &raw).await?;
                let labels = names
                    .resolve(
                        relay.as_ref(),
                        events
                            .iter()
                            .filter(|event| {
                                event.pubkey != scope.agent
                                    && event.pubkey != scope.owner
                                    && event.pubkey != scope.signer.public_key()
                            })
                            .map(|event| event.pubkey),
                    )
                    .await;
                let turns = match events.iter().find(|event| root_for(&scope, event)) {
                    Some(root) => transcripts::load_turns(relay.as_ref(), &scope, root.id).await,
                    None => Vec::new(),
                };
                Ok(Replay {
                    events,
                    labels,
                    turns,
                })
            };
            let result = tokio::time::timeout(HISTORY_TIMEOUT, replay)
                .await
                .unwrap_or_else(|_| Err(problem("session replay timed out; retry")));
            let _ = ops
                .send(Op::HistoryLoaded {
                    id,
                    session: raw,
                    token,
                    prior,
                    result,
                })
                .await;
        });
        Ok(())
    }

    pub(super) async fn finish_history_load(
        &mut self,
        id: Value,
        raw: String,
        token: u64,
        prior: Option<(u64, u64)>,
        result: Result<Replay, CliError>,
    ) -> Result<(), CliError> {
        if self.loading.get(&raw).map(|load| load.token) != Some(token) {
            return self
                .respond_error(id, wire::INVALID_REQUEST, "session load superseded; retry")
                .await;
        }
        let cwd_project = self.loading.get(&raw).and_then(|load| load.project.clone());
        if self
            .loading
            .remove(&raw)
            .is_some_and(|load| load.overflowed)
        {
            return self
                .respond_error(
                    id,
                    wire::INTERNAL_ERROR,
                    "live history overlap exceeded its bound; retry this load",
                )
                .await;
        }
        if self
            .sessions
            .get(&raw)
            .map(|session| (session.generation, session.last_used))
            != prior
            || self
                .sessions
                .get(&raw)
                .is_some_and(|session| session.pending.is_some())
        {
            return self
                .respond_error(
                    id,
                    wire::INVALID_REQUEST,
                    "session changed while loading; retry",
                )
                .await;
        }
        let raw = raw.as_str();
        let Replay {
            events,
            labels,
            turns,
        } = match result {
            Ok(replay) => replay,
            Err(error) => {
                return self
                    .respond_error(id, wire::INVALID_PARAMS, &error.to_string())
                    .await
            }
        };
        let Some((root_id, project)) = events
            .iter()
            .find(|event| root_for(&self.scope, event))
            .map(|event| {
                // An untagged root adopts the client's project, so a later
                // `/fresh` records it on the new root.
                let project =
                    projects::project_of_root(event, &authors(&self.scope)).or(cwd_project);
                (event.id.to_hex(), project)
            })
        else {
            return self
                .respond_error(
                    id,
                    wire::INTERNAL_ERROR,
                    "session history is missing a valid thread root; retry",
                )
                .await;
        };
        if !events.iter().any(|event| involves(&self.scope, event)) {
            return self
                .respond_error(
                    id,
                    wire::INVALID_PARAMS,
                    "session does not involve the configured agent",
                )
                .await;
        }
        // One thread has one session: live replies are routed by root, so a
        // second handle (a saved alias and the thread id) would miss them.
        if let Some(open) = self
            .sessions
            .values()
            .find(|session| session.id != raw && session.root.as_deref() == Some(&root_id))
        {
            let message = format!("this thread is already open as session {}", open.id);
            return self
                .respond_error(id, wire::INVALID_REQUEST, &message)
                .await;
        }
        if !self.sessions.contains_key(raw)
            && self.sessions.len() >= MAX_SESSIONS
            && !self.evict_idle_session()
        {
            return self
                .respond_error(
                    id,
                    wire::INTERNAL_ERROR,
                    "all sessions have running prompts",
                )
                .await;
        }
        let last_used = self.next_tick();
        let own_events = RecentIds::new(OWN_EVENT_CAP);
        self.sessions
            .entry(raw.to_owned())
            .or_insert_with(|| Session {
                id: raw.to_owned(),
                replay_epoch: last_used,
                root: Some(root_id.clone()),
                project,
                ended: None,
                generation: 0,
                last_used,
                own_events,
                streamed: String::new(),
                deferred: Vec::new(),
                deferred_dropped: 0,
                last_prompt: None,
                pending: None,
            });
        if let Some(session) = self.sessions.get_mut(raw) {
            session.last_used = last_used;
            session.replay_epoch = last_used;
            session.root = Some(root_id);
        }
        let mut replayed: HashSet<EventId> = events.iter().map(|event| event.id).collect();
        let root = self
            .sessions
            .get(raw)
            .and_then(|session| session.root.as_deref());
        let overlap = self
            .history_live
            .iter()
            .filter(|event| {
                scoped(&self.scope, event)
                    && buzz_core::nip10::parse_thread_markers(&event.tags)
                        .resolve()
                        .is_some_and(|(thread, _)| Some(thread.as_str()) == root)
                    && replayed.insert(event.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        for event in &events {
            self.seen.insert(event.id);
        }
        for event in &overlap {
            self.seen.insert(event.id);
        }
        for update in replay::updates(
            &self.scope,
            events.into_iter().chain(overlap).collect(),
            labels,
            turns,
            &self.views,
        ) {
            self.send(wire::session_update(raw, update)).await?;
        }
        self.send(wire::ok(id, json!({}))).await
    }
}

async fn load_events<R: BridgeRelay>(
    relay: &R,
    scope: &Scope,
    session_id: &str,
) -> Result<Vec<Event>, CliError> {
    let root_event = resolve_root(relay, scope, session_id).await?;
    let root = root_event.id;
    let mut replay_bytes = root_event.content.len();
    let mut events = vec![root_event];
    let mut filter = json!({"kinds": [9], "#h": [scope.channel.to_string()], "#e": [root.to_hex()], "limit": QUERY_PAGE, "consistency":"strong"});
    let mut previous = None;
    for _ in 0..=MAX_SCAN / QUERY_PAGE {
        let mut page = relay.query(filter.clone()).await?;
        if page.len() > QUERY_PAGE {
            return Err(problem("relay exceeded history page bound"));
        }
        let full = page.len() == QUERY_PAGE;
        newest_first(&mut page);
        if let Some(last) = page.last() {
            let next = cursor(last);
            if previous.as_ref() == Some(&next) {
                return Err(problem("relay did not advance history cursor"));
            }
            apply_cursor(&mut filter, &next)?;
            previous = Some(next);
        }
        for event in page {
            if scoped(scope, &event)
                && buzz_core::nip10::parse_thread_markers(&event.tags)
                    .resolve()
                    .is_some_and(|(id, _)| id == root.to_hex())
            {
                replay_bytes = replay_bytes.saturating_add(event.content.len());
                if replay_bytes > MAX_REPLAY_BYTES {
                    return Err(problem("thread exceeds 4 MiB replay bound"));
                }
                events.push(event);
            }
        }
        if events.len() > MAX_SCAN {
            return Err(problem("thread exceeds 1000-event replay bound"));
        }
        if !full {
            events.sort_by_key(|event| (event.created_at, event.id));
            events.dedup_by_key(|event| event.id);
            if !events.iter().any(|event| involves(scope, event)) {
                return Err(problem(
                    "session does not involve this channel's configured agent",
                ));
            }
            return Ok(events);
        }
    }
    Err(problem("thread history exceeded scan bound"))
}

async fn resolve_root<R: BridgeRelay>(
    relay: &R,
    scope: &Scope,
    session_id: &str,
) -> Result<Event, CliError> {
    if let Ok(id) = EventId::from_hex(session_id) {
        return relay.query(json!({"kinds":[9], "#h":[scope.channel.to_string()], "ids":[id.to_hex()], "limit":1, "consistency":"strong"})).await?
            .into_iter().find(|event| event.id == id && root_for(scope,event))
            .ok_or_else(||problem("session is not a thread for this channel and agent"));
    }
    // Kind 9 has no SQL d_tag column: the relay applies #d after LIMIT.
    // Authors, channel and the single #p are SQL predicates, so scan those
    // bounded candidates and validate the signed alias here instead.
    let alias = format!("buzz-acp:{session_id}");
    let mut filter = alias_filter(scope);
    let mut previous = None;
    for _ in 0..MAX_SCAN / QUERY_PAGE {
        let mut events = relay.query(filter.clone()).await?;
        if events.len() > QUERY_PAGE {
            return Err(problem("relay exceeded session page bound"));
        }
        newest_first(&mut events);
        if let Some(root) = events.iter().find(|event| {
            root_for(scope, event)
                && has_tag(event, "p", &scope.agent.to_hex())
                && authors(scope).contains(&event.pubkey)
                && has_tag(event, "d", &alias)
        }) {
            return Ok(root.clone());
        }
        if events.len() < QUERY_PAGE {
            return Err(problem(
                "session is not a thread for this channel and agent",
            ));
        }
        let next = cursor(
            events
                .last()
                .ok_or_else(|| problem("missing session cursor"))?,
        );
        if previous.as_ref() == Some(&next) {
            return Err(problem("relay did not advance session cursor"));
        }
        apply_cursor(&mut filter, &next)?;
        previous = Some(next);
    }
    Err(problem(
        "session alias exceeds the 1000-event scan bound; load its thread event id",
    ))
}

#[cfg(test)]
#[path = "history_completion_tests.rs"]
mod completion_tests;
