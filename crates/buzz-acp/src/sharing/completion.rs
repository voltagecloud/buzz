//! Terminal outcomes for admitted handoffs. Existing signed returns win over
//! the fallback. Indeterminate reads retain the existing notice journal record.
use super::{Client, Error};
use buzz_sdk::sharing::task;
use nostr::{Event, EventId, PublicKey, Tag, Timestamp};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio_util::task::TaskTracker;

/// Shared by the ACP client and task recovery metadata. Either result handling
/// or panic recovery takes the owner after deciding the batch's actual fate.
#[derive(Clone, Default)]
pub(crate) struct Slot(Arc<Mutex<Option<Arc<State>>>>);
impl Slot {
    pub(crate) fn get(&self) -> Option<Arc<State>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn take(&self) -> Option<Arc<State>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
    fn set(&self, state: Arc<State>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state);
    }
}

pub(crate) struct State {
    pending: Mutex<Vec<Event>>,
    tool_failed: AtomicBool,
    client: Arc<Client>,
    tasks: TaskTracker,
}
impl State {
    fn contains(&self, id: EventId) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|event| event.id == id)
    }

    pub(crate) fn record(&self, message: &Value) {
        let update = &message["params"]["update"];
        if message["method"] == "session/update"
            && matches!(
                update["sessionUpdate"].as_str(),
                Some("tool_call" | "tool_call_update")
            )
            && update["status"] == "failed"
        {
            self.tool_failed.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn finish(&self, requeued: &[EventId]) {
        let pending = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let failed = self.tool_failed.load(Ordering::Relaxed);
        let client = self.client.clone();
        let requeued = requeued.to_vec();
        self.tasks.spawn(async move {
            client.outbox.lock().await.mark_queued(&requeued);
            for trigger in pending {
                if requeued.contains(&trigger.id) { continue; }
                if client.finish_handoff(&trigger, failed).await.is_err() {
                    tracing::error!(code = "handoff_completion_journal_failed", category = "storage",
                        trigger = %trigger.id, "original pending fallback retained for recovery");
                }
            }
            if client.flush_notice().await.is_err() {
                tracing::error!(code = "handoff_completion_delivery_retry", category = "transport",
                    "handoff terminal outcome remains pending");
            }
        });
    }
}

pub(crate) struct Guard {
    state: Arc<State>,
}
impl Guard {
    pub(crate) fn begin(
        acp: &mut crate::acp::AcpClient,
        client: Option<Arc<Client>>,
        batch: Option<&crate::queue::FlushBatch>,
        tasks: TaskTracker,
    ) -> Option<Self> {
        let client = client?;
        let pending = batch?
            .events
            .iter()
            .chain(&batch?.cancelled_events)
            .filter(|item| {
                client.accepts(&item.event) && task::origin(&item.event).ok().flatten().is_some()
            })
            .map(|item| item.event.clone())
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return None;
        }
        let state = Arc::new(State {
            pending: Mutex::new(pending),
            tool_failed: AtomicBool::new(false),
            client: client.clone(),
            tasks,
        });
        acp.handoff_completion.set(state.clone());
        Some(Self { state })
    }
    pub(crate) async fn reserve(&self) -> Result<(), super::notices::ReservationError> {
        let pending = self
            .state
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for trigger in pending {
            let notice = self
                .state
                .client
                .fallback(&trigger, false)
                .map_err(|_| super::notices::ReservationError::Storage)?;
            self.state
                .client
                .outbox
                .lock()
                .await
                .reserve_completion(trigger.id, notice)?;
        }
        Ok(())
    }
}

impl Client {
    /// Run at the main loop's stable queue boundary. Limit reconciliation to
    /// the at-most-32 records whose owners explicitly requeued them. Never wait
    /// for the delivery worker's network lock on the event-processing path.
    pub(crate) fn recover_abandoned(
        &self,
        queue: &crate::queue::EventQueue,
        pool: &crate::pool::AgentPool,
    ) {
        let Ok(mut outbox) = self.outbox.try_lock() else {
            return;
        };
        if outbox
            .recover_abandoned(|id| {
                queue.retains_event(id)
                    || pool.task_map().values().any(|meta| {
                        // Registered before spawn, while the completion slot
                        // remains empty until the task is first polled.
                        meta.recoverable_batch.as_ref().is_some_and(|batch| {
                            batch
                                .events
                                .iter()
                                .chain(&batch.cancelled_events)
                                .any(|item| item.event.id == id)
                        }) || meta
                            .handoff_completion
                            .get()
                            .is_some_and(|state| state.contains(id))
                    })
            })
            .is_err()
        {
            tracing::error!(
                code = "handoff_completion_recovery_retry",
                category = "storage",
                "abandoned handoff record retained for another recovery sweep"
            );
        }
    }

    fn fallback(&self, trigger: &Event, tool_failed: bool) -> Result<Event, Error> {
        let (code, category) = if tool_failed {
            ("tool_failed", "tool")
        } else {
            ("handoff_no_return", "turn")
        };
        let notice = task::handoff(trigger, self.keys.public_key(), trigger.pubkey,
            &format!("Handoff failed: code={code}; category={category}. The turn ended without a return."),
            Timestamp::now().as_secs()).map_err(|_| Error)?
            .tag(Tag::parse(["buzz-handoff-failure", code, category]).map_err(|_| Error)?)
            .sign_with_keys(&self.keys).map_err(|_| Error)?;
        Ok(notice)
    }

    async fn finish_handoff(&self, trigger: &Event, tool_failed: bool) -> Result<(), Error> {
        let notice = self.fallback(trigger, tool_failed)?;
        self.outbox
            .lock()
            .await
            .release_completion(trigger.id, notice)
    }

    pub(super) fn valid_completion(&self, notice: &Event) -> bool {
        let targets: Vec<_> = notice
            .tags
            .iter()
            .filter_map(|tag| {
                let t = tag.as_slice();
                (t.first().map(String::as_str) == Some("p")).then_some(t)
            })
            .collect();
        if targets.len() != 1 || targets[0].len() != 2 {
            return false;
        }
        let Ok(target) = PublicKey::from_hex(&targets[0][1]) else {
            return false;
        };
        task::scope(notice, target, Timestamp::now().as_secs()).ok() == Some(self.scope)
            && task::origin(notice).ok().flatten().is_some()
    }

    async fn events(&self, mut filter: Value) -> Result<Vec<Event>, Error> {
        filter["consistency"] = json!("strong");
        let raw = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.rest.query_raw(&[filter]),
        )
        .await
        .map_err(|_| Error)?
        .map_err(|_| Error)?;
        serde_json::from_value(raw).map_err(|_| Error)
    }

    async fn event(&self, id: EventId) -> Result<Event, Error> {
        let mut events = self
            .events(json!({"kinds":[9,40002],"ids":[id],"#h":[self.scope.channel],"limit":1}))
            .await?;
        if events.len() != 1 {
            return Err(Error);
        }
        let event = events.pop().ok_or(Error)?;
        if event.id != id || event.verify().is_err() {
            return Err(Error);
        }
        Ok(event)
    }

    pub(super) async fn completion_needed(&self, notice: &Event) -> Result<bool, Error> {
        let expected = task::origin(notice).map_err(|_| Error)?.ok_or(Error)?;
        let source = self.event(expected.parent).await?;
        if !self.accepts(&source) {
            return Err(Error);
        }
        let origin = task::origin(&source).map_err(|_| Error)?.ok_or(Error)?;
        if expected.request != origin.request || expected.agent != origin.agent {
            return Err(Error);
        }
        // A result received from a worker must not itself produce another result.
        let parent = self.event(origin.parent).await?;
        if parent.pubkey == self.keys.public_key()
            && task::origin(&parent).map_err(|_| Error)?.is_some()
            && task::scope(&parent, source.pubkey, Timestamp::now().as_secs()).ok()
                == Some(self.scope)
        {
            return Ok(false);
        }
        let results = self
            .events(json!({"kinds":[9,40002],"authors":[self.keys.public_key()],
            "#h":[self.scope.channel],"#e":[source.id],"#p":[source.pubkey],"limit":64}))
            .await?;
        if results.len() >= 64 {
            return Err(Error);
        }
        Ok(!results.iter().any(|event| {
            event.pubkey == self.keys.public_key()
                && task::scope(event, source.pubkey, Timestamp::now().as_secs()).ok()
                    == Some(self.scope)
                && task::origin(event).ok().flatten() == Some(expected)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{
            ws::{Message, WebSocketUpgrade},
            Json,
        },
        routing::{get, post},
        Router,
    };
    use futures_util::StreamExt;
    use nostr::{EventBuilder, Keys, Kind};

    #[tokio::test]
    async fn turn_completion_returns_safe_failure_or_preserves_existing_outcome() {
        for mode in [
            "tool_failed",
            "no_return",
            "returned",
            "requeued",
            "lookup_failed",
            "incoming_return",
            "restart",
            "double_crash",
            "lookup_recovers",
            "capacity_full",
            "storage_failed",
            "returned_double_crash",
        ] {
            let (owner, sender, worker) = (Keys::generate(), Keys::generate(), Keys::generate());
            let now = Timestamp::now().as_secs();
            let channel = uuid::Uuid::new_v4();
            let root = EventBuilder::new(Kind::Custom(9), "private-repository-content")
                .tags([
                    Tag::parse(["h", &channel.to_string()]).unwrap(),
                    Tag::public_key(sender.public_key()),
                ])
                .sign_with_keys(&owner)
                .unwrap();
            let handoff = task::handoff(
                &root,
                sender.public_key(),
                worker.public_key(),
                "private-task",
                now,
            )
            .unwrap()
            .sign_with_keys(&sender)
            .unwrap();
            let result = task::handoff(
                &handoff,
                worker.public_key(),
                sender.public_key(),
                "done",
                now,
            )
            .unwrap()
            .sign_with_keys(&worker)
            .unwrap();
            let (agent, source) = if mode == "incoming_return" {
                (sender.clone(), result.clone())
            } else {
                (worker.clone(), handoff.clone())
            };
            let mut events = vec![
                root.clone(),
                handoff,
                EventBuilder::new(Kind::Custom(39000), "")
                    .tags([
                        Tag::parse(["d", &channel.to_string()]).unwrap(),
                        Tag::parse(["name", "test"]).unwrap(),
                    ])
                    .sign_with_keys(&owner)
                    .unwrap(),
            ];
            if matches!(
                mode,
                "returned" | "incoming_return" | "returned_double_crash"
            ) {
                events.push(result);
            }
            let fixtures = Arc::new(events);
            let writes = Arc::new(Mutex::new(Vec::<Event>::new()));
            let capture = writes.clone();
            let query = fixtures.clone();
            let fail = Arc::new(AtomicBool::new(matches!(
                mode,
                "lookup_failed" | "lookup_recovers"
            )));
            let query_fails = fail.clone();
            let manager = owner.clone();
            let app = Router::new()
                .route(
                    "/query",
                    post(move |Json(filters): Json<Vec<Value>>| {
                        let events = query.clone();
                        let fail = query_fails.clone();
                        async move {
                            if filters[0]["kinds"] == json!([9, 40002]) {
                                assert_eq!(filters[0]["consistency"], "strong");
                            }
                            let filter: nostr::Filter =
                                serde_json::from_value(filters[0].clone()).unwrap();
                            Json(if fail.load(Ordering::Relaxed) {
                                Vec::new()
                            } else {
                                events
                                    .iter()
                                    .filter(|event| {
                                        filter.match_event(
                                            event,
                                            nostr::filter::MatchEventOptions::default(),
                                        )
                                    })
                                    .cloned()
                                    .collect::<Vec<_>>()
                            })
                        }
                    }),
                )
                .route(
                    "/",
                    get(move |ws: WebSocketUpgrade| {
                        let capture = capture.clone();
                        let manager = manager.clone();
                        async move {
                            ws.on_upgrade(move |mut socket| async move {
                                socket
                                    .send(Message::Text(json!(["AUTH", "test"]).to_string().into()))
                                    .await
                                    .unwrap();
                                while let Some(Ok(Message::Text(text))) = socket.next().await {
                                    let frame: Value = serde_json::from_str(&text).unwrap();
                                    if frame[0] == "REQ" {
                                        socket
                                            .send(Message::Text(
                                                json!(["EOSE", frame[1]]).to_string().into(),
                                            ))
                                            .await
                                            .unwrap();
                                        continue;
                                    }
                                    let is_return = frame[0] == "EVENT" && frame[1]["kind"] == 9;
                                    if is_return {
                                        let event: Event =
                                            serde_json::from_value(frame[1].clone()).unwrap();
                                        capture.lock().unwrap().push(event);
                                    }
                                    let accepted = !(mode == "double_crash"
                                        && is_return
                                        && capture.lock().unwrap().len() < 3);
                                    let id = frame[1]["id"].clone();
                                    socket
                                        .send(Message::Text(
                                            json!(["OK", id, accepted, ""]).to_string().into(),
                                        ))
                                        .await
                                        .unwrap();
                                    if frame[0] == "EVENT" {
                                        let event: Event =
                                            serde_json::from_value(frame[1].clone()).unwrap();
                                        event.verify().unwrap();
                                        if event.kind.as_u16() != 9 {
                                            let opened = buzz_session_control::open_request(
                                                &event, &manager,
                                            )
                                            .unwrap();
                                            let reply = buzz_session_control::Reply::done(
                                                opened.request.request_id,
                                                json!({"ok":true}),
                                            );
                                            let reply = buzz_session_control::build_reply(
                                                &manager,
                                                &opened.signer,
                                                &event.id,
                                                &reply,
                                                Timestamp::now(),
                                            )
                                            .unwrap();
                                            socket
                                                .send(Message::Text(
                                                    json!(["EVENT", "shared-reply", reply])
                                                        .to_string()
                                                        .into(),
                                                ))
                                                .await
                                                .unwrap();
                                        }
                                    }
                                }
                            })
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let directory = tempfile::tempdir().unwrap();
            let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
            let outbox_directory = directory.path().join("outbox");
            let wire = directory.path().join("acp-wire");
            let client = Arc::new(Client {
                keys: agent.clone(),
                auth: auth.clone(),
                relay: format!("ws://{addr}"),
                manager: owner.public_key(),
                scope: task::Scope {
                    channel,
                    root: root.id,
                },
                rest: crate::relay::RestClient {
                    http: reqwest::Client::new(),
                    base_url: format!("http://{addr}"),
                    keys: agent,
                    auth_tag_json: Some(auth),
                },
                outbox: Arc::new(tokio::sync::Mutex::new(super::super::notices::Outbox::new(
                    outbox_directory.clone(),
                ))),
            });
            if mode == "capacity_full" {
                for i in 0..32 {
                    client
                        .outbox
                        .lock()
                        .await
                        .reserve(EventId::from_byte_array([i; 32]), source.clone())
                        .unwrap();
                }
            } else if mode == "storage_failed" {
                std::fs::write(&outbox_directory, "not a directory").unwrap();
            }
            let batch = crate::queue::FlushBatch {
                channel_id: channel,
                scope: crate::scope::SessionScope::Thread {
                    channel_id: channel,
                    root_event_id: root.id.to_hex(),
                },
                events: vec![crate::queue::BatchEvent {
                    event: source.clone(),
                    prompt_tag: String::new(),
                    received_at: std::time::Instant::now(),
                    edit: None,
                }],
                cancelled_events: vec![],
                cancel_reason: None,
            };
            let tasks = TaskTracker::new();
            let mut acp = if matches!(
                mode,
                "tool_failed" | "no_return" | "capacity_full" | "storage_failed"
            ) {
                let notification = if mode == "tool_failed" {
                    r#"printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","status":"failed","rawInput":"super-secret","rawOutput":"private-repository-content"}}}';"#
                } else {
                    ""
                };
                let script = format!(
                    r#"while IFS= read -r line; do echo received >> '{}'; {notification} printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"stopReason":"end_turn"}}}}'; done"#,
                    wire.display()
                );
                crate::acp::AcpClient::spawn("bash", &["-c".into(), script], &[], false)
                    .await
                    .unwrap()
            } else {
                crate::acp::AcpClient::spawn("cat", &[], &[], false)
                    .await
                    .unwrap()
            };
            if matches!(
                mode,
                "tool_failed" | "no_return" | "capacity_full" | "storage_failed"
            ) {
                let mut state = crate::pool::SessionState::default();
                state
                    .sessions
                    .insert(batch.scope.clone(), "test-session".into());
                let agent = crate::pool::OwnedAgent {
                    index: 0,
                    acp,
                    state,
                    model_capabilities: None,
                    desired_model: None,
                    model_overridden: false,
                    desired_model_request_id: None,
                    desired_model_pending_ack: false,
                    startup_effort: None,
                    agent_name: "legacy-test-agent".into(),
                    goose_system_prompt_supported: None,
                    protocol_version: 1,
                };
                let mut ctx = crate::pool::tests::make_prompt_context_no_owner();
                ctx.agent_keys = client.keys.clone();
                ctx.rest_client = client.rest.clone();
                ctx.channel_info =
                    crate::pool::ChannelInfoResolver::new(Default::default(), client.rest.clone());
                ctx.sharing = Some(client.clone());
                ctx.dedup_mode = crate::config::DedupMode::Queue;
                ctx.transcript_tasks = tasks.clone();
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    crate::pool::run_prompt_task(
                        agent,
                        Some(batch),
                        None,
                        Arc::new(ctx),
                        tx,
                        None,
                        "terminal-test".into(),
                        Default::default(),
                    ),
                )
                .await
                .unwrap();
                let result = rx.recv().await.unwrap();
                if matches!(mode, "capacity_full" | "storage_failed") {
                    let reason = if mode == "capacity_full" {
                        "handoff completion capacity is full; retry this task"
                    } else {
                        "handoff completion storage is unavailable; retry this task"
                    };
                    assert!(
                        matches!(&result.outcome, crate::pool::PromptOutcome::Error(crate::acp::AcpError::HandoffAdmissionRetry(text)) if text == reason)
                    );
                    assert!(!wire.exists(), "refused admission must never reach ACP");
                    assert_eq!(result.batch.as_ref().unwrap().events[0].event.id, source.id);
                } else {
                    assert!(matches!(result.outcome, crate::pool::PromptOutcome::Ok(_)));
                    assert!(wire.exists());
                }
                acp = result.agent.acp;
                acp.handoff_completion.take().unwrap().finish(&[]);
            } else {
                let guard =
                    Guard::begin(&mut acp, Some(client.clone()), Some(&batch), tasks.clone())
                        .unwrap();
                guard.reserve().await.unwrap();
                if matches!(mode, "restart" | "double_crash" | "returned_double_crash") {
                    let saved = std::fs::read(outbox_directory.join(format!("{}.json", source.id)))
                        .unwrap();
                    let first: Value = serde_json::from_slice(&saved).unwrap();
                    assert_eq!(first["phase"], "pending");
                    assert!(client.outbox.lock().await.next(u64::MAX).unwrap().is_none());
                    acp.handoff_completion.take();
                    for crash in 0..if matches!(mode, "double_crash" | "returned_double_crash") {
                        3
                    } else {
                        1
                    } {
                        // Crash after publication but before record deletion. A second
                        // crash during recovery must preserve the exact signed ID.
                        if mode != "double_crash" {
                            std::fs::write(
                                outbox_directory.join(format!("{}.json", source.id)),
                                &saved,
                            )
                            .unwrap();
                        }
                        *client.outbox.lock().await =
                            super::super::notices::Outbox::new(outbox_directory.clone());
                        client.flush_notice().await.unwrap();
                        if mode == "double_crash" && crash < 2 {
                            let path = outbox_directory.join(format!("{}.json", source.id));
                            let mut actual: Value =
                                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                            assert_eq!(actual["phase"], "ready");
                            assert_eq!(actual["attempts"], crash + 1);
                            // Advance only the retry clock, preserving actual
                            // durable event identity, phase, and attempt count.
                            actual["next_try"] = json!(0);
                            std::fs::write(path, serde_json::to_vec(&actual).unwrap()).unwrap();
                        }
                    }
                }
                if mode == "requeued" {
                    let state = acp.handoff_completion.take().unwrap();
                    state.finish(&[source.id]);
                }
                drop(guard);
                if let Some(state) = acp.handoff_completion.take() {
                    state.finish(&[]);
                }
            }
            tasks.close();
            tokio::time::timeout(std::time::Duration::from_secs(15), tasks.wait())
                .await
                .unwrap();
            if mode == "lookup_recovers" {
                let path = outbox_directory.join(format!("{}.json", source.id));
                for _ in 0..10 {
                    let mut record: Value =
                        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                    assert_eq!(record["attempts"], 0);
                    record["next_try"] = json!(0);
                    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
                    client.flush_notice().await.unwrap();
                }
                fail.store(false, Ordering::Relaxed);
                let mut record: Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                record["next_try"] = json!(0);
                std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
                client.flush_notice().await.unwrap();
            }
            let emitted = writes.lock().unwrap().clone();
            let expected = matches!(
                mode,
                "tool_failed" | "no_return" | "restart" | "double_crash" | "lookup_recovers"
            );
            assert_eq!(
                emitted.len(),
                if mode == "double_crash" {
                    3
                } else {
                    usize::from(expected)
                },
                "{mode}"
            );
            if mode == "double_crash" {
                assert!(
                    emitted.iter().all(|event| event == &emitted[0]),
                    "recovery replays exact signed bytes"
                );
            }
            if expected {
                let notice = &emitted[0];
                assert!(notice.content.contains(if mode == "tool_failed" {
                    "code=tool_failed; category=tool"
                } else {
                    "code=handoff_no_return; category=turn"
                }));
                assert!(!notice.content.contains("super-secret"));
                assert!(!notice.content.contains("private-repository-content"));
                assert_eq!(task::origin(notice).unwrap().unwrap().parent, source.id);
                assert_eq!(
                    task::scope(notice, source.pubkey, now).unwrap(),
                    client.scope
                );
            }
            assert_eq!(
                std::fs::read_dir(&outbox_directory)
                    .map(|entries| entries.count())
                    .unwrap_or(0),
                if mode == "capacity_full" {
                    32
                } else {
                    usize::from(matches!(mode, "lookup_failed" | "requeued"))
                },
                "{mode}"
            );
            acp.shutdown().await;
            server.abort();
        }
    }
}

#[cfg(test)]
pub(crate) mod testing;
