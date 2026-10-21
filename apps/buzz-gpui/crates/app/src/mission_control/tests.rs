use super::*;
use crate::agent_activity::Record;
use crate::agent_activity::turns::{ActivityTurn, parse_time};
use core::prelude::v1::test;
use serde_json::{Value, json};
use std::sync::Arc;

const T0: &str = "2026-10-10T12:00:00Z";
const T1: &str = "2026-10-10T12:00:20Z";

fn t0() -> u64 {
    parse_time(T0).unwrap()
}

fn agent_key() -> String {
    "a".repeat(64)
}

fn event(kind: &str, at: &str, payload: Value) -> Value {
    json!({"kind": kind, "turnId": "t1", "channelId": "c1", "timestamp": at, "payload": payload})
}

fn update(at: &str, update: Value) -> Value {
    event(
        "acp_read",
        at,
        json!({"method": "session/update", "params": {"update": update}}),
    )
}

fn snapshot(events: Vec<Value>, state: TurnState, evidence: u64) -> Snapshot {
    let plaintext = json!({"kind": "batch", "payload": {"events": events}});
    Snapshot {
        status: FeedStatus::Listening,
        records: vec![Arc::new(Record {
            id: "r1".into(),
            agent: agent_key(),
            created_at: evidence / 1000,
            plaintext: plaintext.to_string().into(),
            received_at: evidence,
            historical: false,
            kind: "batch".into(),
            channel_ids: vec!["c1".into()],
        })]
        .into(),
        turns: vec![ActivityTurn {
            agent: agent_key(),
            turn_id: "t1".into(),
            channel_id: Some("c1".into()),
            timestamp: evidence,
            state,
            requests: None,
        }]
        .into(),
        ..Default::default()
    }
}

fn live_in(snapshot: &Snapshot, origin: &str, agent: &str, now: u64) -> Option<Live> {
    evidence_in(snapshot, origin, agent).map(|evidence| evidence.at(now))
}

#[test]
fn cached_evidence_still_ages_out_of_the_time_windows() {
    let snap = snapshot(
        vec![event("turn_started", T0, json!({}))],
        TurnState::Working,
        t0() + 25_000,
    );
    let evidence = evidence_in(&snap, "o", &agent_key()).unwrap();
    let soon = evidence.at(t0() + 30_000);
    assert!(soon.turn.is_some());
    assert_eq!(soon.recent_turns, 1);
    let abandoned = evidence.at(t0() + 25_000 + ABANDONED_MS);
    assert!(abandoned.turn.is_none());
    assert_eq!(abandoned.recent_turns, 1);
    assert_eq!(evidence.at(t0() + USAGE_WINDOW_MS).recent_turns, 0);
}

fn running(since: u64) -> Run {
    Run {
        status: Status::Ready,
        since: Some(since),
        failed: None,
    }
}

#[test]
fn an_open_turn_names_its_running_tool_and_progress() {
    let snap = snapshot(
        vec![
            event("turn_started", T0, json!({})),
            update(
                T1,
                json!({"sessionUpdate": "tool_call", "toolCallId": "x", "title": "Read", "kind": "read", "status": "in_progress", "locations": [{"path": "/src/main.rs"}]}),
            ),
        ],
        TurnState::Working,
        t0() + 25_000,
    );
    let live = live_in(&snap, "wss://relay", &agent_key(), t0() + 30_000).unwrap();
    let turn = live.turn.as_ref().unwrap();
    assert_eq!(turn.label, "Reading main.rs");
    assert_eq!(turn.started_at, t0());
    assert_eq!(turn.progress_at, t0() + 20_000);
    assert!(turn.tool_running && !turn.waiting_permission);
    assert_eq!(
        live.place,
        Some(("wss://relay".to_string(), "c1".to_string()))
    );
    let run = running(t0() - 60_000);
    let now = t0() + 30_000;
    let shown = doing(&run, Some(&live), Link::Connected, now);
    assert_eq!(
        (shown.text.as_str(), shown.elapsed.as_deref(), shown.active),
        ("Reading main.rs", Some("30s"), true)
    );
    assert_eq!(attention(&run, Some(&live), Link::Connected, now), None);
}

#[test]
fn a_quiet_turn_is_stuck_after_the_limit() {
    let snap = snapshot(
        vec![
            event("turn_started", T0, json!({})),
            update(
                T1,
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Looking"}}),
            ),
        ],
        TurnState::Working,
        // Heartbeats keep coming, but nothing else does.
        t0() + 20 * 60_000,
    );
    let run = running(t0() - 60_000);
    let progress = t0() + 20_000;
    let before = progress + STUCK_MS - 1000;
    let live = live_in(&snap, "o", &agent_key(), before).unwrap();
    assert_eq!(live.turn.as_ref().unwrap().label, "Writing a reply");
    assert_eq!(attention(&run, Some(&live), Link::Connected, before), None);
    let after = progress + STUCK_MS;
    let stuck = attention(&run, Some(&live), Link::Connected, after).unwrap();
    assert_eq!(stuck.need, Need::Stuck);
    assert_eq!(stuck.reason, "No progress for 5m 0s");
    // Silence while the feed is interrupted proves nothing.
    let deaf = Live {
        listening: false,
        ..live.clone()
    };
    assert_eq!(attention(&run, Some(&deaf), Link::Connected, after), None);
}

#[test]
fn a_running_tool_gets_longer_before_it_is_stuck() {
    let live = Live {
        turn: Some(OpenTurn {
            started_at: 0,
            progress_at: 0,
            evidence_at: 0,
            label: "Running a command".into(),
            waiting_permission: false,
            tool_running: true,
        }),
        listening: true,
        ..Default::default()
    };
    let run = running(0);
    assert_eq!(
        attention(&run, Some(&live), Link::Connected, STUCK_MS),
        None
    );
    assert_eq!(
        attention(&run, Some(&live), Link::Connected, TOOL_STUCK_MS).map(|a| a.need),
        Some(Need::Stuck)
    );
}

#[test]
fn a_pending_permission_waits_on_the_owner() {
    let snap = snapshot(
        vec![
            event("turn_started", T0, json!({})),
            event(
                "acp_read",
                T1,
                json!({"method": "session/request_permission", "id": 9, "params": {"toolCall": {"toolCallId": "e1", "title": "Edit main.rs"}, "options": [{"optionId": "ok", "name": "Allow once"}]}}),
            ),
        ],
        TurnState::Working,
        t0() + 20_000,
    );
    let now = t0() + 21_000;
    let live = live_in(&snap, "o", &agent_key(), now).unwrap();
    assert_eq!(live.turn.as_ref().unwrap().label, "Waiting for permission");
    let run = running(0);
    let waiting = attention(&run, Some(&live), Link::Connected, now).unwrap();
    assert_eq!(
        (waiting.need, waiting.reason.as_str()),
        (Need::Waiting, "Waiting for permission")
    );
}

#[test]
fn a_failed_last_turn_needs_attention_for_a_while() {
    let snap = snapshot(
        vec![
            event("turn_started", T0, json!({})),
            event(
                "turn_error",
                T1,
                json!({"error": "rate limited\nretry later"}),
            ),
        ],
        TurnState::Ended,
        t0() + 20_000,
    );
    let now = t0() + 60_000;
    let live = live_in(&snap, "o", &agent_key(), now).unwrap();
    assert!(live.turn.is_none());
    let run = running(0);
    let failed = attention(&run, Some(&live), Link::Connected, now).unwrap();
    assert_eq!(
        (failed.need, failed.reason.as_str()),
        (Need::Failed, "Turn failed: rate limited")
    );
    let later = t0() + 20_000 + TURN_ERROR_MS;
    assert_eq!(attention(&run, Some(&live), Link::Connected, later), None);
    assert_eq!(
        doing(&run, Some(&live), Link::Connected, now).text,
        "Idle · last active just now"
    );
}

#[test]
fn runner_states_map_to_needs() {
    let run = |status, since, failed: Option<(u64, &str)>| Run {
        status,
        since,
        failed: failed.map(|(at, error)| (at, error.to_string())),
    };
    let now = 60 * 60_000;
    let need = |run: &Run, link| attention(run, None, link, now);
    assert_eq!(
        need(
            &run(Status::Ready, None, Some((now - 1000, "exit 1\ntrace"))),
            Link::Connected
        ),
        Some(Attention {
            need: Need::Failed,
            reason: "Run failed: exit 1".into()
        })
    );
    assert_eq!(
        need(
            &run(Status::Ready, None, Some((now - TURN_ERROR_MS, "old"))),
            Link::Connected
        ),
        None,
        "a failure long ago is history"
    );
    assert_eq!(need(&run(Status::Ready, None, None), Link::Connected), None);
    let connecting = run(Status::Connecting, Some(now - 60_000), None);
    assert_eq!(
        need(&connecting, Link::Connected),
        None,
        "a minute of connecting is normal"
    );
    assert_eq!(
        need(
            &run(Status::Connecting, Some(now - 10 * 60_000), None),
            Link::Connected
        )
        .map(|a| a.reason),
        Some("Connecting for 10m 0s".into())
    );
    assert_eq!(
        need(
            &run(Status::Working, Some(now - TOOL_STUCK_MS), None),
            Link::Connected
        )
        .map(|a| a.need),
        Some(Need::Stuck)
    );
    assert_eq!(
        need(
            &run(Status::Working, Some(now - 60_000), None),
            Link::Connected
        ),
        None
    );
    // An unreachable host's last state proves nothing.
    assert_eq!(
        need(
            &run(Status::Ready, None, Some((now, "exit 1"))),
            Link::Offline
        ),
        None
    );
    let shown = doing(
        &run(Status::Working, Some(now - 30_000), None),
        None,
        Link::Connected,
        now,
    );
    assert_eq!(
        (shown.text.as_str(), shown.elapsed.as_deref(), shown.active),
        ("Working", Some("30s"), true)
    );
    assert_eq!(
        doing(
            &run(Status::Elsewhere, None, None),
            None,
            Link::Connected,
            now
        )
        .text,
        "Runs in another app"
    );
    assert_eq!(
        doing(&run(Status::Ready, None, None), None, Link::Offline, now).text,
        "Last known: ready"
    );
}

fn entry(name: &str, host: u64, status: Status, need: Option<Need>) -> Entry {
    Entry {
        key: Key {
            host,
            id: name.to_lowercase(),
        },
        host_label: format!("host {host}"),
        remote: host > 0,
        pubkey: String::new(),
        name: name.into(),
        picture: None,
        model: None,
        status,
        link: Link::Connected,
        attention: need.map(|need| Attention {
            need,
            reason: String::new(),
        }),
        doing: Doing::default(),
        usage: None,
        place: None,
    }
}

#[test]
fn attention_sorts_first_then_work_then_elsewhere_then_offline() {
    let mut working = entry("Busy", 0, Status::Working, None);
    working.doing.active = true;
    let mut offline = entry("Aardvark", 2, Status::Ready, None);
    offline.link = Link::Offline;
    let mut entries = vec![
        offline,
        entry("Zed", 0, Status::Elsewhere, None),
        entry("Idle", 1, Status::Ready, None),
        entry("Stuck", 1, Status::Working, Some(Need::Stuck)),
        working,
        entry("Boot", 0, Status::Connecting, None),
        entry("Asks", 0, Status::Ready, Some(Need::Waiting)),
        entry("broken", 1, Status::Ready, Some(Need::Failed)),
        entry("Alpha", 0, Status::Elsewhere, None),
    ];
    sort(&mut entries);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "broken", "Asks", "Stuck", "Busy", "Idle", "Alpha", "Zed", "Boot", "Aardvark"
        ]
    );
}

#[test]
fn notifications_fire_on_transitions_after_the_baseline() {
    let mut tracker = Tracker::default();
    let failed = entry("A", 1, Status::Ready, Some(Need::Failed));
    let calm = entry("B", 1, Status::Ready, None);
    // What is wrong when a host first answers is the baseline.
    assert!(
        tracker
            .observe(&[failed.clone(), calm.clone()], false)
            .is_empty()
    );
    assert!(
        tracker
            .observe(&[failed.clone(), calm.clone()], false)
            .is_empty()
    );
    // B starts waiting: one alert, once.
    let waiting = entry("B", 1, Status::Ready, Some(Need::Waiting));
    let alerts = tracker.observe(&[failed.clone(), waiting.clone()], false);
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].name, "B");
    assert!(
        tracker
            .observe(&[failed.clone(), waiting.clone()], false)
            .is_empty()
    );
    // Waiting turns into stuck: a new need alerts again.
    let stuck = entry("B", 1, Status::Ready, Some(Need::Stuck));
    assert_eq!(tracker.observe(&[failed.clone(), stuck], false).len(), 1);
    // The host goes offline and comes back unchanged: no news.
    let mut gone = failed.clone();
    gone.link = Link::Offline;
    assert!(tracker.observe(&[gone, calm.clone()], false).is_empty());
    assert!(
        tracker
            .observe(&[failed.clone(), calm.clone()], false)
            .is_empty()
    );
    // Recovering and failing again is news; while quiet it is only noted.
    let fine = entry("A", 1, Status::Ready, None);
    assert!(tracker.observe(&[fine, calm.clone()], false).is_empty());
    assert!(
        tracker
            .observe(&[failed.clone(), calm.clone()], true)
            .is_empty()
    );
    assert!(tracker.observe(&[failed, calm], false).is_empty());
    // A second host has its own baseline.
    let other = entry("C", 2, Status::Ready, Some(Need::Failed));
    assert!(tracker.observe(&[other], false).is_empty());
}

#[test]
fn communities_merge_to_the_newest_evidence() {
    let older = Live {
        last_seen: Some(10),
        place: Some(("one".into(), "c1".into())),
        failed_turn: Some((10, "boom".into())),
        recent_turns: 2,
        listening: true,
        ..Default::default()
    };
    let newer = Live {
        last_seen: Some(20),
        place: Some(("two".into(), "c2".into())),
        recent_turns: 1,
        ..Default::default()
    };
    let merged = merge([older, newer]).unwrap();
    assert_eq!(merged.place, Some(("two".into(), "c2".into())));
    assert_eq!(merged.failed_turn, Some((10, "boom".into())));
    assert_eq!((merged.recent_turns, merged.listening), (3, true));
}

fn metric(id: &str, agent: &str, turn: &str, at: &str, tokens: u64) -> ArchiveRecord {
    ArchiveRecord {
        id: id.into(),
        agent: agent.into(),
        created_at: 0,
        received_at: 0,
        plaintext: json!({
            "channelId": "c1", "harness": "goose", "timestamp": at,
            "sessionId": "s1", "turnId": turn, "turnSeq": 1,
            "turn": {"totalTokens": tokens, "costUsd": 0.25}
        })
        .to_string(),
    }
}

#[test]
fn usage_counts_recent_turns_once() {
    let since = parse_time("2026-10-10T00:00:00Z").unwrap() as i64;
    let records = vec![
        metric("1", "a", "t1", "2026-10-10T10:00:00Z", 1200),
        // The same turn delivered twice counts once.
        metric("2", "a", "t1", "2026-10-10T10:00:00Z", 1200),
        metric("3", "a", "t2", "2026-10-10T11:00:00Z", 800),
        metric("4", "a", "t0", "2026-10-09T11:00:00Z", 5000),
        metric("5", "b", "t9", "2026-10-10T11:00:00Z", 10),
    ];
    let usage = usage_by_agent(&records, since);
    assert_eq!(
        usage["a"],
        Usage {
            turns: 2,
            tokens: Some(2000),
            cost: Some(0.5)
        }
    );
    assert_eq!(
        usage_line(usage.get("a"), None).as_deref(),
        Some("2 turns · 2k tokens · $0.50")
    );
    assert_eq!(
        usage_line(usage.get("b"), None).as_deref(),
        Some("1 turn · 10 tokens · $0.25")
    );
    let live = Live {
        recent_turns: 3,
        ..Default::default()
    };
    assert_eq!(usage_line(None, Some(&live)).as_deref(), Some("3 turns"));
    assert_eq!(usage_line(None, None), None);
}

#[test]
fn numbers_and_times_read_compactly() {
    assert_eq!(compact(950), "950");
    assert_eq!(compact(12_340), "12.3k");
    assert_eq!(compact(2_000), "2k");
    assert_eq!(compact(999_999), "1M");
    assert_eq!(compact(4_500_000), "4.5M");
    assert_eq!(ago(0, 30_000), "just now");
    assert_eq!(ago(0, 5 * 60_000), "5m ago");
    assert_eq!(ago(0, 3 * 3_600_000), "3h ago");
    assert_eq!(ago(0, 49 * 3_600_000), "2d ago");
    assert_eq!(first_line("\n  first  \nsecond"), "first");
}
