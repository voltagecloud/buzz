use crate::error_outcome_emission_tests::{dummy_agent, test_config};
use crate::pool::{AgentPool, PromptOutcome, PromptResult, PromptSource, TimeoutKind};
use crate::queue::{BatchEvent, FlushBatch};
use crate::*;
use nostr::{EventBuilder, Keys, Kind};

/// hard-cap timeout dead-letters immediately (no requeue); idle timeout is requeued.
#[tokio::test]
async fn hard_timeout_not_requeued_idle_timeout_is_requeued() {
    let make_batch = || {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(9), "test")
            .sign_with_keys(&keys)
            .unwrap();
        let __cid = Uuid::new_v4();
        FlushBatch {
            channel_id: __cid,
            scope: scope::SessionScope::Conversation { channel_id: __cid },
            events: vec![BatchEvent {
                edit: None,
                event,
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
            }],
            cancelled_events: vec![],
            cancel_reason: None,
        }
    };

    // Returns (pending_channels, queued_event_count_for_channel).
    let run = |outcome: PromptOutcome, batch: FlushBatch| async move {
        let channel_id = batch.channel_id;
        let agent = dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: None,
                scope: None,
                turn_id: "test-turn-id".to_string(),
                recoverable_batch: None,
                handoff_completion: Default::default(),
                control_tx: None,
                steer_tx: None,
                successful_steer_deliveries: HashSet::new(),
            },
        );
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let config = test_config();
        let mut heartbeat_in_flight = false;
        let removed_channels = HashSet::new();
        let mut crash_history = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (respawn_tx, _respawn_rx) = mpsc::channel(8);
        let mut respawn_tasks = tokio::task::JoinSet::new();
        let result = PromptResult {
            agent,
            source: PromptSource::Channel(scope::SessionScope::Conversation { channel_id }),
            turn_id: "test-turn-id".to_string(),
            outcome,
            batch: Some(batch),
        };
        handle_prompt_result(
            &mut pool,
            &mut queue,
            &config,
            result,
            &mut heartbeat_in_flight,
            &removed_channels,
            &mut crash_history,
            &respawn_tx,
            &mut respawn_tasks,
            None,
            None,
            &mut tokio::time::Instant::now(),
        );
        (
            queue.pending_channels(),
            queue.queued_event_count(channel_id),
        )
    };

    // Hard timeout (not recently active): dead-lettered immediately.
    let hard_batch = make_batch();
    let (hard_channels, hard_events) = run(
        PromptOutcome::Timeout(TimeoutKind::Hard {
            recently_active: false,
        }),
        hard_batch,
    )
    .await;
    assert_eq!(
        hard_channels, 0,
        "hard-cap timeout (not recently active) must not requeue the batch"
    );
    assert_eq!(
        hard_events, 0,
        "hard-cap timeout (not recently active) must drop all events"
    );

    // Idle timeout: batch IS requeued (first attempt, not yet dead-lettered).
    let idle_batch = make_batch();
    let (idle_channels, idle_events) =
        run(PromptOutcome::Timeout(TimeoutKind::Idle), idle_batch).await;
    assert_eq!(
        idle_channels, 1,
        "idle timeout must requeue the batch for retry"
    );
    assert_eq!(
        idle_events, 1,
        "idle timeout must preserve the event for retry"
    );
}

#[tokio::test]
async fn pending_owner_follows_actual_result_and_panic_queue_fates() {
    for mode in [
        "auth",
        "model",
        "hard",
        "active_retry",
        "active_exhausted",
        "retry",
        "exhausted",
        "cancel",
        "cancel_drain",
        "removed",
        "panic_retry",
        "panic_exhausted",
    ] {
        let mut agent = dummy_agent(0).await;
        let directory = tempfile::tempdir().unwrap();
        let fixture =
            crate::sharing::completion::testing::Handoff::prepare(&mut agent.acp, directory.path())
                .await;
        let completion = agent.acp.handoff_completion.clone();
        let mut batch = fixture.batch.clone();
        // Cancelled carryover owns the same record across another merge.
        if matches!(mode, "cancel" | "cancel_drain") {
            batch.cancelled_events = std::mem::take(&mut batch.events);
            batch.cancel_reason = Some(CancelReason::Steer);
        }
        let scope = batch.scope.clone();
        let mut pool = AgentPool::from_slots(vec![None]);
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        if mode.contains("exhausted") {
            queue.set_retry_count_for_test(scope.clone(), 99);
        }
        let task = if mode.starts_with("panic") {
            pool.join_set.spawn(async { panic!("fixture panic") })
        } else {
            pool.join_set.spawn(async {})
        };
        pool.task_map_mut().insert(
            task.id(),
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: Some(batch.channel_id),
                scope: Some(scope.clone()),
                turn_id: "fate-test".into(),
                recoverable_batch: Some(batch.clone()),
                handoff_completion: completion,
                control_tx: None,
                steer_tx: None,
                successful_steer_deliveries: HashSet::new(),
            },
        );
        let config = test_config();
        let mut heartbeat = false;
        let removed = if mode == "removed" {
            HashSet::from([batch.channel_id])
        } else {
            HashSet::new()
        };
        let mut circuits = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (tx, _rx) = mpsc::channel(8);
        let mut respawns = tokio::task::JoinSet::new();
        if mode.starts_with("panic") {
            let error = pool.join_set.join_next().await.unwrap().unwrap_err();
            recover_panicked_agent(
                &mut pool,
                &mut queue,
                &config,
                error,
                &mut heartbeat,
                &removed,
                &mut HashMap::new(),
                &mut circuits,
                &tx,
                &mut respawns,
                None,
            );
            agent.acp.shutdown().await;
        } else {
            let outcome = match mode {
                "auth" => PromptOutcome::Error(acp::AcpError::AgentError {
                    code: -32000,
                    message: "API Error: 401".into(),
                }),
                "model" => PromptOutcome::Error(acp::AcpError::AgentError {
                    code: -32002,
                    message: "model not found".into(),
                }),
                "hard" => PromptOutcome::Timeout(TimeoutKind::Hard {
                    recently_active: false,
                }),
                "active_retry" | "active_exhausted" => PromptOutcome::Timeout(TimeoutKind::Hard {
                    recently_active: true,
                }),
                "cancel" => PromptOutcome::Cancelled,
                "cancel_drain" => {
                    PromptOutcome::CancelDrainTimeout(std::time::Duration::from_secs(5))
                }
                _ => PromptOutcome::Error(acp::AcpError::AgentError {
                    code: -32000,
                    message: "retry".into(),
                }),
            };
            handle_prompt_result(
                &mut pool,
                &mut queue,
                &config,
                PromptResult {
                    agent,
                    source: PromptSource::Channel(scope),
                    turn_id: "fate-test".into(),
                    outcome,
                    batch: Some(batch),
                },
                &mut heartbeat,
                &removed,
                &mut circuits,
                &tx,
                &mut respawns,
                None,
                None,
                &mut tokio::time::Instant::now(),
            );
        }
        fixture
            .assert_terminal(matches!(
                mode,
                "auth"
                    | "model"
                    | "hard"
                    | "active_exhausted"
                    | "exhausted"
                    | "removed"
                    | "panic_exhausted"
            ))
            .await;
        respawns.abort_all();
        while respawns.join_next().await.is_some() {}
    }
}

#[tokio::test]
async fn disposed_requeued_handoffs_recover_without_restart() {
    for mode in ["cap", "drain", "retained", "inflight", "unpolled"] {
        let mut agent = dummy_agent(0).await;
        let directory = tempfile::tempdir().unwrap();
        let fixture =
            crate::sharing::completion::testing::Handoff::prepare(&mut agent.acp, directory.path())
                .await;
        let id = fixture.batch.events[0].event.id;
        let completion = agent.acp.handoff_completion.clone();
        // Mark the owner as queued through the same transition as result handling.
        completion.take().unwrap().finish(&[id]);
        fixture.assert_terminal(false).await;
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let mut pool = AgentPool::from_slots(vec![None]);
        assert!(queue.requeue(fixture.batch.clone()).is_none());
        if mode == "cap" {
            for i in 0..500 {
                queue.push(crate::queue::QueuedEvent {
                    channel_id: fixture.batch.channel_id,
                    scope: fixture.batch.scope.clone(),
                    event: EventBuilder::new(Kind::Custom(9), format!("new work {i}"))
                        .sign_with_keys(&Keys::generate())
                        .unwrap(),
                    received_at: std::time::Instant::now(),
                    prompt_tag: String::new(),
                    edit: None,
                });
            }
        } else if mode == "drain" {
            queue.drain_channel(fixture.batch.channel_id);
        } else if mode == "inflight" {
            queue.drain_channel(fixture.batch.channel_id);
            fixture.resume_with_late_queue_marker(&mut agent.acp).await;
            let task = pool.join_set.spawn(async {});
            pool.task_map_mut().insert(
                task.id(),
                crate::pool::TaskMeta {
                    agent_index: 0,
                    channel_id: Some(fixture.batch.channel_id),
                    scope: Some(fixture.batch.scope.clone()),
                    turn_id: "active".into(),
                    recoverable_batch: None,
                    handoff_completion: agent.acp.handoff_completion.clone(),
                    control_tx: None,
                    steer_tx: None,
                    successful_steer_deliveries: HashSet::new(),
                },
            );
            fixture.recover_abandoned(&queue, &pool);
            fixture.assert_terminal(false).await;
        }
        if mode == "unpolled" {
            queue.drain_channel(fixture.batch.channel_id);
            // Same synchronous metadata as dispatch_pending before Guard::begin.
            // No await between registering the spawned task and sweeping.
            let task = pool.join_set.spawn(std::future::pending());
            pool.task_map_mut().insert(
                task.id(),
                crate::pool::TaskMeta {
                    agent_index: 0,
                    channel_id: Some(fixture.batch.channel_id),
                    scope: Some(fixture.batch.scope.clone()),
                    turn_id: "unpolled".into(),
                    recoverable_batch: Some(fixture.batch.clone()),
                    handoff_completion: Default::default(),
                    control_tx: None,
                    steer_tx: None,
                    successful_steer_deliveries: HashSet::new(),
                },
            );
        }
        fixture.recover_abandoned(&queue, &pool);
        fixture
            .assert_terminal(matches!(mode, "cap" | "drain"))
            .await;
        agent.acp.shutdown().await;
    }
}
