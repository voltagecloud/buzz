use super::turns::TurnState;
use super::*;
use serde_json::json;

const NOW: u64 = 1_791_633_610_000;
const T0: &str = "2026-10-10T12:00:00Z";

fn agent(c: char) -> String {
    c.to_string().repeat(64)
}

fn frame(id: char, from: char, plaintext: Value) -> Frame {
    Frame {
        id: id.to_string().repeat(64),
        agent: agent(from),
        created_at: NOW / 1000,
        plaintext: plaintext.to_string(),
    }
}

fn all(_: &str) -> bool {
    true
}

fn started(turn: &str, channel: &str, at: &str) -> Value {
    json!({"kind": "turn_started", "turnId": turn, "channelId": channel, "timestamp": at, "payload": {}})
}

fn listening() -> Activity {
    let mut activity = Activity::new(true, false);
    activity.activate(NOW);
    activity.set_status(FeedStatus::Listening, NOW);
    activity
}

#[test]
fn frames_are_checked() {
    let ok =
        json!({"id": "a".repeat(64), "agent": "b".repeat(64), "createdAt": 1, "plaintext": "{}"});
    assert!(Frame::parse(&ok).is_some());
    let upper =
        json!({"id": "A".repeat(64), "agent": "b".repeat(64), "createdAt": 1, "plaintext": "{}"});
    assert!(Frame::parse(&upper).is_none());
    let not_json =
        json!({"id": "a".repeat(64), "agent": "b".repeat(64), "createdAt": 1, "plaintext": "nope"});
    assert!(Frame::parse(&not_json).is_none());
    let large = json!({"id": "a".repeat(64), "agent": "b".repeat(64), "createdAt": 1,
        "plaintext": format!("\"{}\"", "x".repeat(PLAINTEXT_BYTES))});
    assert!(Frame::parse(&large).is_none());
}

#[test]
fn leases_start_and_stop_the_feed() {
    let mut activity = Activity::new(true, true);
    assert_eq!(activity.snapshot().status, FeedStatus::Disabled);
    let read = activity
        .activate(NOW)
        .expect("the first lease reads history");
    assert_eq!(read.before, None);
    assert_eq!(activity.snapshot().status, FeedStatus::Connecting);
    assert_eq!(activity.snapshot().history, HistoryStatus::Loading);
    assert!(activity.activate(NOW).is_none());
    assert!(!activity.release(NOW));
    assert!(activity.release(NOW), "the last lease stops it");
    assert_eq!(activity.snapshot().status, FeedStatus::Disabled);
    // A host that cannot decode stays unavailable.
    let mut none = Activity::new(false, false);
    assert!(none.activate(NOW).is_none());
    assert_eq!(none.snapshot().status, FeedStatus::Unavailable);
}

#[test]
fn turns_work_until_stale_ended_or_interrupted() {
    let mut activity = listening();
    activity.receive(frame('1', 'a', started("t1", "c1", T0)), NOW, &all);
    let at = turns::parse_time(T0).unwrap();
    let state = |activity: &Activity| activity.snapshot().turns[0].state;
    activity.publish(at + 1_000);
    assert_eq!(state(&activity), TurnState::Working);
    assert_eq!(&*activity.snapshot().working_channels, ["c1".to_string()]);
    activity.publish(at + turns::FRESH_MS + 1);
    assert_eq!(state(&activity), TurnState::Unknown, "stale");
    activity.publish(at + 1_000);
    assert_eq!(state(&activity), TurnState::Working);
    // An interruption makes earlier evidence unknown, even after reconnect.
    activity.set_status(FeedStatus::Interrupted, at + 1_000);
    assert_eq!(state(&activity), TurnState::Unknown);
    activity.set_status(FeedStatus::Listening, at + 1_000);
    assert_eq!(state(&activity), TurnState::Unknown);
    // Completion ends it; a late heartbeat cannot revive it.
    let done = json!({"kind": "turn_completed", "turnId": "t1", "channelId": "c1", "timestamp": "2026-10-10T12:00:02Z"});
    activity.receive(frame('2', 'a', done), NOW, &all);
    let late = json!({"kind": "turn_liveness", "turnId": "t1", "channelId": "c1", "timestamp": "2026-10-10T12:00:03Z"});
    activity.receive(frame('3', 'a', late), NOW, &all);
    assert_eq!(state(&activity), TurnState::Ended);
}

#[test]
fn denied_channels_drop_the_whole_frame() {
    let mut activity = listening();
    let mixed = json!({"kind": "batch", "payload": {"events": [started("t1", "c1", T0), started("t2", "secret", T0)]}});
    activity.receive(frame('1', 'a', mixed), NOW, &|c| c != "secret");
    assert!(activity.snapshot().records.is_empty());
    assert!(activity.snapshot().turns.is_empty());
    let management = json!({"kind": "agent_management_request", "payload": {}});
    activity.receive(frame('2', 'a', management), NOW, &all);
    assert!(activity.snapshot().records.is_empty());
}

#[test]
fn the_live_window_is_bounded() {
    let mut activity = listening();
    for n in 0..RECORD_LIMIT + 5 {
        let mut f = frame('1', 'a', json!({"kind": "turn_liveness"}));
        f.id = format!("{n:064x}");
        activity.receive(f, NOW + n as u64, &all);
    }
    assert_eq!(activity.snapshot().records.len(), RECORD_LIMIT);
    assert_eq!(activity.snapshot().trimmed, 5);
    // Records age out after 90 days.
    activity.publish(NOW + HISTORY_AGE_MS + 1_000);
    assert!(activity.snapshot().records.is_empty());
    // The byte budget applies too.
    let mut activity = listening();
    let big = format!("\"{}\"", "x".repeat(60_000));
    for n in 0..40u64 {
        let f = Frame {
            id: format!("{n:064x}"),
            agent: agent('a'),
            created_at: 1,
            plaintext: big.clone(),
        };
        activity.receive(f, NOW + n, &all);
    }
    let bytes: usize = activity
        .snapshot()
        .records
        .iter()
        .map(|r| r.plaintext.len())
        .sum();
    assert!(bytes <= BYTE_LIMIT && activity.snapshot().records.len() < 40);
}

#[test]
fn turn_states_are_bounded_with_a_floor() {
    let mut activity = listening();
    for n in 0..turns::TURN_LIMIT + 1 {
        let at = chrono::DateTime::from_timestamp_millis((NOW - 100_000 + n as u64) as i64)
            .unwrap()
            .to_rfc3339();
        let mut f = frame('1', 'a', started(&format!("t{n}"), "c1", &at));
        f.id = format!("{n:064x}");
        activity.receive(f, NOW, &all);
    }
    assert_eq!(activity.snapshot().turns.len(), turns::TURN_LIMIT);
    // The forgotten oldest turn cannot come back with old evidence.
    let old = chrono::DateTime::from_timestamp_millis((NOW - 100_000) as i64)
        .unwrap()
        .to_rfc3339();
    activity.receive(frame('f', 'a', started("t0", "c1", &old)), NOW, &all);
    assert!(!activity.snapshot().turns.iter().any(|t| t.turn_id == "t0"));
}

#[test]
fn typing_counts_only_for_agents_with_telemetry() {
    let mut activity = listening();
    let entries = vec![
        (agent('a'), "c1".to_string(), None),
        (agent('b'), "c1".to_string(), None),
        (agent('a'), "c2".to_string(), Some("r".repeat(64))),
    ];
    activity.typing(&entries, NOW, &all);
    assert!(activity.snapshot().typing.is_empty(), "no telemetry yet");
    activity.receive(frame('1', 'a', json!({"kind": "turn_liveness"})), NOW, &all);
    activity.typing(&entries, NOW, &all);
    let typing = &activity.snapshot().typing;
    assert_eq!(typing.len(), 2);
    assert_eq!(typing[0].started_at, NOW);
    // Thread typing does not make the channel working.
    assert_eq!(&*activity.snapshot().working_channels, ["c1".to_string()]);
    // A continuing run keeps its start; it expires without pulses.
    activity.typing(&entries, NOW + 3_000, &all);
    assert_eq!(activity.snapshot().typing[0].started_at, NOW);
    activity.publish(NOW + 3_000 + turns::TYPING_FRESH_MS);
    assert!(activity.snapshot().typing.is_empty());
}

#[test]
fn typing_asked_again_soon_leaves_the_snapshot_alone() {
    let mut activity = listening();
    let entries = vec![(agent('a'), "c1".to_string(), None)];
    activity.receive(frame('1', 'a', json!({"kind": "turn_liveness"})), NOW, &all);
    activity.typing(&entries, NOW, &all);
    let before = activity.snapshot().clone();
    activity.typing(&entries, NOW + 1_000, &all);
    assert_eq!(*activity.snapshot(), before);
    // Past half its life, a continuing run is renewed.
    let later = NOW + turns::TYPING_FRESH_MS / 2 + 1_000;
    activity.typing(&entries, later, &all);
    let typing = &activity.snapshot().typing[0];
    assert_eq!(
        (typing.started_at, typing.expires_at),
        (NOW, later + turns::TYPING_FRESH_MS)
    );
}

#[test]
fn history_pages_replace_only_the_saved_window() {
    let mut activity = Activity::new(true, true);
    let read = activity.activate(NOW).unwrap();
    activity.set_status(FeedStatus::Listening, NOW);
    activity.receive(frame('1', 'a', json!({"kind": "turn_liveness"})), NOW, &all);
    let page = Page {
        records: vec![
            (
                frame('3', 'b', json!({"kind": "x", "channelId": "c1"})),
                NOW - 2_000,
            ),
            (
                frame('2', 'b', json!({"kind": "y", "channelId": "gone"})),
                NOW - 3_000,
            ),
        ],
        skipped: 1,
        agents: vec![agent('b')],
        before: Some(42),
    };
    activity.history_loaded(read.generation, Ok(page), NOW, &|c| c != "gone");
    let snapshot = activity.snapshot().clone();
    assert_eq!(snapshot.history, HistoryStatus::Ready);
    assert_eq!(
        snapshot.records.len(),
        2,
        "one saved record was not restorable"
    );
    assert!(snapshot.records[0].historical);
    assert!(snapshot.has_older && !snapshot.history_older);
    assert_eq!(snapshot.history_skipped, 1);
    assert_eq!(&*snapshot.history_agents, [agent('b')]);
    // Saved records are not working evidence.
    assert!(snapshot.turns.is_empty());

    let older = activity.load_older(NOW).unwrap();
    assert_eq!(older.before, Some(42));
    activity.history_loaded(older.generation, Ok(Page::default()), NOW, &all);
    let snapshot = activity.snapshot();
    assert!(snapshot.history_older && !snapshot.has_older);
    assert_eq!(snapshot.records.len(), 1, "live records stay");
    let latest = activity.latest_history(NOW).unwrap();
    assert_eq!(latest.before, None);
    let other = activity.select_history(&agent('b'), NOW).unwrap();
    assert_eq!(other.agent, Some(agent('b')));
    assert!(activity.select_history(&agent('b'), NOW).is_none());
    // The answer for the earlier selection is stale.
    activity.history_loaded(latest.generation, Err("old".into()), NOW, &all);
    assert_eq!(activity.snapshot().history, HistoryStatus::Loading);
}

#[test]
fn pages_parse_from_the_decoder() {
    let value = json!({"records": [
        {"id": "a".repeat(64), "agent": "b".repeat(64), "createdAt": 5, "plaintext": "{}", "receivedAt": 5000},
        {"id": "bad"}
    ], "skipped": 2, "agents": ["b".repeat(64), "nope"], "before": 7, "revision": 1});
    let page = Page::parse(&value).unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].1, 5000);
    assert_eq!((page.skipped, page.before), (2, Some(7)));
    assert_eq!(page.agents, vec!["b".repeat(64)]);
}

#[test]
fn targets_round_trip_and_refuse_ambiguity() {
    let a = agent('a');
    let root = agent('f');
    let link = target(&a, Some("chan nel"), Some(&root));
    assert_eq!(
        selection(&link),
        Some(Selection {
            agent: a.clone(),
            channel: Some("chan nel".into()),
            thread: Some(root.clone()),
        })
    );
    assert_eq!(
        target(&a, None, Some(&root)),
        format!("buzz:agent-activity?agent={a}")
    );
    assert!(selection(&format!("buzz:agent-activity?agent={a}")).is_some());
    for bad in [
        format!("buzz:agent-activity?agent={a}&agent={a}"),
        format!("buzz:agent-activity?agent={a}&thread={root}"),
        format!("buzz:agent-activity?agent={a}&channel="),
        format!("buzz:agent-activity?agent={a}&other=1"),
        format!("buzz:agent-activity?agent={a}#x"),
        format!("buzz://agent-activity?agent={a}"),
        format!("buzz:agent-activity?agent={}", "A".repeat(64)),
        format!("https:agent-activity?agent={a}"),
    ] {
        assert_eq!(selection(&bad), None, "{bad}");
    }
}

#[test]
fn raw_records_split_batches_in_a_channel() {
    let mut activity = listening();
    let batch = json!({"kind": "batch", "payload": {"events": [
        {"kind": "turn_started", "channelId": "c1"},
        {"kind": "acp_read", "channelId": "c2"},
        {"kind": "acp_write"}
    ]}});
    activity.receive(frame('1', 'a', batch), NOW, &all);
    activity.receive(
        frame('2', 'a', json!({"kind": "x", "channelId": "c2"})),
        NOW,
        &all,
    );
    activity.receive(
        frame('3', 'b', json!({"kind": "x", "channelId": "c2"})),
        NOW,
        &all,
    );
    let records = &activity.snapshot().records;
    assert_eq!(records[0].channel_ids, vec!["c1", "c2"]);
    assert_eq!(raw_records(records, &agent('a'), None).len(), 2);
    let c2 = raw_records(records, &agent('a'), Some("c2"));
    assert_eq!(c2.len(), 2);
    assert_eq!(c2[0].id, format!("{}:1", "1".repeat(64)));
    assert_eq!(c2[0].envelope_id, "1".repeat(64));
    assert_eq!(c2[0].kind, "acp_read");
    assert!(c2[0].plaintext.contains("\n  \"channelId\": \"c2\""));
}
