use super::super::local::{Begin, LocalActivity, WorkOutcome};
use super::super::turns::TriggerMessage;
use super::super::{Activity, Frame};
use super::*;
use serde_json::json;

const NOW: u64 = 1_791_633_610_000;
const T0: &str = "2026-10-10T12:00:00Z";
const SCOPE: &str = "https://relay.test:viewer";

fn key(c: char) -> String {
    c.to_string().repeat(64)
}

struct Fixture {
    activity: Activity,
    seq: u64,
}

impl Fixture {
    fn new() -> Self {
        let mut activity = Activity::new(true, false);
        activity.activate(NOW);
        activity.set_status(FeedStatus::Listening, NOW);
        Self { activity, seq: 0 }
    }

    fn send(&mut self, kind: &str, turn: &str, trigger: &str) {
        self.seq += 1;
        let plaintext = json!({
            "kind": kind, "turnId": turn, "seq": self.seq, "channelId": "channel",
            "timestamp": T0, "payload": {"triggeringEventIds": [trigger]},
        });
        self.activity.receive(
            Frame {
                id: format!("{:064x}", self.seq),
                agent: key('a'),
                created_at: NOW / 1000,
                plaintext: plaintext.to_string(),
            },
            NOW,
            &|_| true,
        );
    }

    fn message(&mut self, id: &str, root: &str) {
        self.activity.messages_found(
            &[TriggerMessage {
                id: id.into(),
                kind: 9,
                channels: vec!["channel".into()],
                references: true,
                root: Some(root.into()),
            }],
            NOW,
        );
    }

    fn snapshot(&self) -> &Snapshot {
        self.activity.snapshot()
    }

    fn bubble(&self, id: &str) -> Vec<RequestWork> {
        message_activity(self.snapshot(), "channel", id)
    }

    fn thread(&self, typing: &[&str]) -> Vec<String> {
        let typing: Vec<String> = typing.iter().map(|t| t.to_string()).collect();
        thread_working_agents(self.snapshot(), &typing, "channel", &key('b'), None)
    }
}

#[test]
fn one_request_status_drives_bubbles_and_thread_summaries() {
    let (agent, request, sibling) = (key('a'), key('c'), key('d'));
    let mut f = Fixture::new();
    f.send("turn_started", "one", &request);
    f.send("turn_started", "two", &sibling);
    assert_eq!(
        f.activity.unresolved(),
        vec![request.clone(), sibling.clone()]
    );
    f.message(&request, &key('b'));
    f.message(&sibling, &key('b'));
    assert!(f.activity.unresolved().is_empty());
    f.send("turn_liveness", "one", &request);
    assert!(f.bubble(&request)[0].working);
    assert_eq!(f.thread(&[]), vec![agent.clone()]);
    f.send("turn_completed", "one", &request);
    assert!(f.bubble(&request).is_empty());
    assert!(f.bubble(&sibling)[0].working);
    assert_eq!(f.thread(&[&agent]), vec![agent.clone()]);
    f.send("turn_completed", "two", &sibling);
    assert!(f.bubble(&sibling).is_empty());
    assert_eq!(
        f.thread(&[&agent, "pulse-only"]),
        vec!["pulse-only".to_string()]
    );
    // A settled request cannot be revived by liveness.
    f.send("turn_liveness", "one", &request);
    assert_eq!(f.thread(&[]), Vec::<String>::new());
}

#[test]
fn duplicate_starts_leave_the_request_unknown() {
    let request = key('c');
    let mut f = Fixture::new();
    f.send("turn_started", "one", &request);
    f.send("turn_started", "one", &request);
    let selected = request_activity(&f.snapshot().records, &key('a'), "channel", &[&request]);
    assert!(selected.uncertain);
    assert!(
        request_activity(&f.snapshot().records, &key('a'), "channel", &["short"])
            .turn_ids
            .is_empty()
    );
}

fn with_local(local: &LocalActivity, allowed: bool) -> Activity {
    let mut activity = Activity::new(false, false);
    let rows: Vec<LocalWork> = local
        .work()
        .iter()
        .filter(|row| row.scope == SCOPE && allowed && row.channel_id == "channel")
        .cloned()
        .collect();
    activity.set_local(rows.into(), NOW);
    activity
}

#[test]
fn projects_local_work_into_channel_thread_and_message() {
    let (agent, root, reply) = (key('a'), key('b'), key('c'));
    let local = LocalActivity::default();
    let begin = |scope: &str, message: &str| {
        local.begin(
            Begin {
                scope: scope.into(),
                agent: agent.clone(),
                channel_id: "channel".into(),
                message_id: message.into(),
                thread_root_id: root.clone(),
            },
            NOW,
        )
    };
    begin("other", &root);
    let work = begin(SCOPE, &reply);
    let snapshot = |allowed| with_local(&local, allowed).snapshot().clone();
    let s = snapshot(true);
    assert_eq!(s.local.len(), 1);
    assert_eq!(&*s.working_channels, ["channel".to_string()]);
    assert_eq!(
        super::super::current::working_agents(&s, "channel"),
        vec![agent.clone()]
    );
    assert_eq!(
        thread_working_agents(&s, &[], "channel", &root, None),
        vec![agent.clone()]
    );
    assert!(thread_working_agents(&s, &[], "channel", &reply, None).is_empty());
    assert_eq!(
        message_activity(&s, "channel", &reply)[0].state,
        WorkState::Waiting
    );
    assert!(message_activity(&s, "channel", &root).is_empty());
    assert!(thread_waiting(
        &s,
        std::slice::from_ref(&agent),
        "channel",
        &root,
        None
    ));
    work.started();
    assert!(message_activity(&snapshot(true), "channel", &reply)[0].working);
    work.finish(WorkOutcome::failed("Provider unavailable"));
    let s = snapshot(true);
    assert!(super::super::current::working_agents(&s, "channel").is_empty());
    assert!(
        thread_working_agents(&s, std::slice::from_ref(&agent), "channel", &root, None).is_empty()
    );
    let failed = &message_activity(&s, "channel", &reply)[0];
    assert_eq!(failed.state, WorkState::Error);
    assert_eq!(failed.reason.as_deref(), Some("Provider unavailable"));
    local.dismiss_error(SCOPE, &agent, &reply);
    assert!(message_activity(&snapshot(true), "channel", &reply).is_empty());
    assert!(snapshot(false).local.is_empty());
}

#[test]
fn a_bubble_represents_its_agents_in_the_thread_summary() {
    let (agent, root) = (key('a'), key('b'));
    let local = LocalActivity::default();
    local.begin(
        Begin {
            scope: SCOPE.into(),
            agent: agent.clone(),
            channel_id: "channel".into(),
            message_id: root.clone(),
            thread_root_id: root.clone(),
        },
        NOW,
    );
    let s = with_local(&local, true).snapshot().clone();
    let shown: HashSet<String> = [agent.clone()].into();
    assert!(thread_working_agents(&s, &[], "channel", &root, Some((&root, &shown))).is_empty());
    assert_eq!(bubbled_agents(&s, "channel", None), vec![agent.clone()]);
    assert_eq!(bubbled_agents(&s, "channel", Some(&root)), vec![agent]);
}

#[test]
fn a_recovered_session_shows_working_over_the_failure_it_replaced() {
    let (agent, root, mention) = (key('a'), key('b'), key('c'));
    let local = LocalActivity::default();
    // The start for this mention was refused (say, session_ended).
    local
        .begin(
            Begin {
                scope: SCOPE.into(),
                agent: agent.clone(),
                channel_id: "channel".into(),
                message_id: mention.clone(),
                thread_root_id: root.clone(),
            },
            NOW,
        )
        .finish(WorkOutcome::failed("session_ended"));
    let mut f = Fixture::new();
    f.message(&mention, &root);
    f.activity.set_local(local.work(), NOW);
    assert_eq!(f.bubble(&mention)[0].state, WorkState::Error);

    // Recover works; the recovered turn replays the same mention.
    local.resolve_thread(SCOPE, &agent, "channel", Some(&root));
    f.activity.set_local(local.work(), NOW);
    f.send("turn_started", "recovered", &mention);
    f.send("turn_liveness", "recovered", &mention);
    assert!(f.bubble(&mention)[0].working);
    assert_eq!(f.thread(&[&agent]), vec![agent.clone()]);
}

#[test]
fn a_snapshot_remembers_its_request_work_until_replaced() {
    let (request, root) = (key('c'), key('b'));
    let mut f = Fixture::new();
    f.send("turn_started", "one", &request);
    f.message(&request, &root);
    f.send("turn_liveness", "one", &request);
    let s = f.snapshot();
    assert_eq!(
        request_work(s, "channel", &[&request]),
        work_out(s, "channel", &[&request])
    );
    // A remembered answer is served without reading records again.
    s.memo
        .work
        .lock()
        .unwrap()
        .insert(("channel".into(), vec![request.clone()]), Vec::new());
    assert!(f.bubble(&request).is_empty());
    assert!(bubbled_agents(f.snapshot(), "channel", None).is_empty());
    // New activity replaces the snapshot, and with it the memo.
    f.send("turn_liveness", "one", &request);
    assert!(f.bubble(&request)[0].working);
    assert_eq!(
        bubbled_agents(f.snapshot(), "channel", None),
        vec![key('a')]
    );
}

#[test]
fn message_works_are_shared_until_activity_changes() {
    let request = key('c');
    let mut f = Fixture::new();
    f.send("turn_started", "one", &request);
    f.send("turn_liveness", "one", &request);
    let first = message_works(f.snapshot(), "channel");
    let id = EventId::from_hex(&request).unwrap();
    assert!(first[&id][0].working);
    assert_eq!(first.len(), 1);
    assert!(Arc::ptr_eq(&first, &message_works(f.snapshot(), "channel")));
    assert!(message_works(f.snapshot(), "elsewhere").is_empty());
    f.send("turn_completed", "one", &request);
    let after = message_works(f.snapshot(), "channel");
    assert!(!Arc::ptr_eq(&first, &after));
    assert!(after.is_empty());
}

#[test]
fn a_memo_is_bounded_and_never_copied_or_compared() {
    let request = key('c');
    let mut f = Fixture::new();
    f.send("turn_started", "one", &request);
    let s = f.snapshot();
    for n in 0..MEMO_LIMIT + 10 {
        request_work(s, &format!("channel-{n}"), &[&request]);
    }
    assert!(s.memo.work.lock().unwrap().len() <= MEMO_LIMIT);
    let copy = s.clone();
    assert!(copy.memo.work.lock().unwrap().is_empty());
    assert_eq!(&copy, s);
}

/// A record as `admit` builds it; plaintext that is not JSON keeps no
/// events, as no read of it could parse it.
fn record(n: u64, agent: &str, plaintext: &str) -> Arc<Record> {
    let raw = serde_json::from_str(plaintext).unwrap_or(Value::Null);
    let frame = Frame {
        id: format!("{n:064x}"),
        agent: agent.into(),
        created_at: NOW / 1000,
        plaintext: plaintext.into(),
    };
    Arc::new(Record::new(frame, &raw, NOW + n, false))
}

/// Kind, turn, seq, admitted and triggers, as read from the JSON.
type Reread = (String, String, Option<i64>, bool, Vec<String>);

/// `request_activity` as it read the JSON before records kept their
/// events: the Raw view's entries, each parsed again.
fn reread(records: &[Arc<Record>], agent: &str, channel: &str, ids: &[&str]) -> Selected {
    let mut selected = Selected::default();
    let wanted: HashSet<&str> = ids
        .iter()
        .copied()
        .filter(|id| super::super::hex64(id))
        .collect();
    if wanted.is_empty() {
        return selected;
    }
    let parsed: Vec<Reread> = super::super::raw_records(records, agent, Some(channel))
        .into_iter()
        .filter_map(|record| {
            let value: Value = serde_json::from_str(&record.plaintext).ok()?;
            let turn = value.get("turnId")?.as_str()?;
            if turn.is_empty() || turn.len() > 256 {
                return None;
            }
            let admitted = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_time)
                .is_some_and(|at| at <= record.received_at + 5000);
            let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
            let seq = value
                .get("seq")
                .and_then(Value::as_i64)
                .filter(|seq| *seq >= 0 && *seq < (1 << 53));
            let triggers = value
                .get("payload")
                .and_then(|p| p.get("triggeringEventIds"))
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| id.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            Some((kind.into(), turn.into(), seq, admitted, triggers))
        })
        .collect();
    for (kind, start, from, admitted, triggers) in &parsed {
        if kind != "turn_started" || !triggers.iter().any(|id| wanted.contains(id.as_str())) {
            continue;
        }
        let turn: Vec<_> = parsed.iter().filter(|p| p.1 == *start).collect();
        if turn.iter().filter(|p| p.0 == "turn_started").count() != 1
            || turn.iter().any(|p| p.2.is_none())
        {
            selected.uncertain = true;
            continue;
        }
        let Some(from) = *from else { continue };
        let after: Vec<i64> = turn
            .iter()
            .filter_map(|p| p.2)
            .filter(|s| *s >= from)
            .collect();
        if after.iter().collect::<HashSet<_>>().len() != after.len() {
            selected.uncertain = true;
            continue;
        }
        selected.turn_ids.insert(start.clone());
        if !admitted {
            selected.uncertain = true;
            continue;
        }
        let end = turn.iter().find(|p| {
            matches!(
                p.0.as_str(),
                "turn_completed" | "turn_error" | "agent_panic"
            ) && p.3
        });
        if let Some(end) = end
            && end.2.is_some_and(|seq| seq >= from)
        {
            selected
                .terminal
                .insert(start.clone(), end.0 == "turn_completed");
        }
    }
    selected
}

fn frame(kind: &str, turn: Value, seq: Value, channel: &str, at: &str, triggers: Value) -> Value {
    json!({
        "kind": kind, "turnId": turn, "seq": seq, "channelId": channel,
        "timestamp": at, "payload": {"triggeringEventIds": triggers},
    })
}

#[test]
fn selection_reads_batch_children_in_their_own_channel() {
    let (request, other) = (key('c'), key('d'));
    let mut f = Fixture::new();
    let batch = json!({"kind": "batch", "payload": {"events": [
        frame("turn_started", json!("one"), json!(1), "channel", T0, json!([request])),
        // The same turn elsewhere is another channel's business.
        frame("turn_started", json!("one"), json!(1), "elsewhere", T0, json!([request])),
        frame("turn_started", json!("two"), json!(1), "elsewhere", T0, json!([other])),
        // Only a start's triggers are kept.
        frame("turn_completed", json!("one"), json!(2), "channel", T0, json!([request])),
    ]}});
    f.activity.receive(
        Frame {
            id: key('9'),
            agent: key('a'),
            created_at: NOW / 1000,
            plaintext: batch.to_string(),
        },
        NOW,
        &|_| true,
    );
    let records = &f.snapshot().records;
    let selected = request_activity(records, &key('a'), "channel", &[&request]);
    assert_eq!(selected.turn_ids, ["one".to_string()].into());
    assert_eq!(selected.terminal, [("one".to_string(), true)].into());
    assert!(!selected.uncertain);
    assert!(
        request_activity(records, &key('a'), "channel", &[&other])
            .turn_ids
            .is_empty()
    );
    let elsewhere = request_activity(records, &key('a'), "elsewhere", &[&other]);
    assert_eq!(elsewhere.turn_ids, ["two".to_string()].into());
    assert!(elsewhere.terminal.is_empty());
    assert_eq!(
        &*f.snapshot().requested,
        [
            ("channel".to_string(), request.clone()),
            ("elsewhere".to_string(), request.clone()),
            ("elsewhere".to_string(), other.clone()),
        ]
    );
    // Only events selection can read are kept.
    assert_eq!(records[0].events.len(), 4);
    assert!(records[0].events[3].triggers.is_empty());
}

#[test]
fn selection_matches_reading_the_json_on_every_edge() {
    let (a, b) = (key('a'), key('b'));
    let (c, d, e) = (key('c'), key('d'), key('e'));
    let (f, g, h) = (key('f'), key('7'), key('8'));
    // The batch below arrives at NOW + 7: 12:00:10.007.
    let late = "2026-10-10T12:00:16Z";
    let timed = |turn: &str, at: &str, trigger: &str| {
        [
            frame(
                "turn_started",
                json!(turn),
                json!(1),
                "channel",
                at,
                json!([trigger]),
            ),
            frame(
                "turn_completed",
                json!(turn),
                json!(2),
                "channel",
                T0,
                json!([]),
            ),
        ]
    };
    let one = |kind: &str, turn: &str, seq: Value, triggers: Value| {
        frame(kind, json!(turn), seq, "channel", T0, triggers)
    };
    let mut plaintexts = vec![
        one("turn_started", "plain", json!(1), json!([c])).to_string(),
        one("turn_completed", "plain", json!(2), json!([])).to_string(),
        "not json".to_string(),
        "[1, 2]".to_string(),
        json!({"kind": "batch"}).to_string(),
        json!({"kind": "batch", "payload": {"events": "no"}}).to_string(),
        // A turn id on a batch envelope is not an event.
        json!({"kind": "batch", "turnId": "plain", "channelId": "channel", "seq": 9}).to_string(),
    ];
    let mut children = vec![
        one("turn_started", "batched", json!(1), json!([1, d, null])),
        one("turn_error", "batched", json!(3), json!([])),
        one("turn_completed", "batched", json!(4), json!([])),
        json!({"kind": "turn_started", "turnId": "nochannel", "seq": 1, "payload": {"triggeringEventIds": [e]}}),
        json!({"turnId": "nokind", "seq": 1, "channelId": "channel", "timestamp": T0}),
        one("turn_started", "", json!(1), json!([e])),
        one("turn_started", &"x".repeat(257), json!(1), json!([e])),
        one("turn_started", &"y".repeat(256), json!(1), json!([e])),
        one("turn_started", "negative", json!(-1), json!([e])),
        one("turn_started", "huge", json!(1u64 << 53), json!([e])),
        one("turn_started", "edge", json!((1u64 << 53) - 1), json!([e])),
        one("turn_started", "fraction", json!(1.5), json!([e])),
        one("turn_started", "repeat", json!(1), json!([e])),
        one("turn_liveness", "repeat", json!(1), json!([])),
        frame(
            "turn_started",
            json!("late"),
            json!(1),
            "channel",
            late,
            json!([g]),
        ),
        frame(
            "turn_started",
            json!("untimed"),
            json!(1),
            "channel",
            "soon",
            json!([e]),
        ),
        frame(
            "turn_started",
            json!(7),
            json!(1),
            "channel",
            T0,
            json!([e]),
        ),
        json!("not an object"),
    ];
    children.extend(timed("ontime", "2026-10-10T12:00:15.007Z", &f));
    children.extend(timed("overdue", "2026-10-10T12:00:15.008Z", &h));
    plaintexts.push(json!({"kind": "batch", "payload": {"events": children}}).to_string());
    plaintexts.push(one("turn_started", "plain", json!(5), json!(c)).to_string());
    let mut records: Vec<Arc<Record>> = plaintexts
        .iter()
        .enumerate()
        .map(|(n, text)| record(n as u64, &a, text))
        .collect();
    records.push(record(
        99,
        &b,
        &one("turn_started", "theirs", json!(1), json!([c])).to_string(),
    ));
    let id_sets: [&[&str]; 9] = [
        &[&c],
        &[&d],
        &[&e],
        &[&f],
        &[&g],
        &[&h],
        &[&c, &d, &e, &f, &g, &h],
        &["short"],
        &[],
    ];
    let mut seen = Selected::default();
    for agent in [&a, &b] {
        for channel in ["channel", "elsewhere"] {
            for ids in id_sets {
                let selected = request_activity(&records, agent, channel, ids);
                assert_eq!(
                    selected,
                    reread(&records, agent, channel, ids),
                    "{channel} {ids:?}"
                );
                seen.turn_ids.extend(selected.turn_ids);
                seen.terminal.extend(selected.terminal);
                seen.uncertain |= selected.uncertain;
            }
        }
    }
    // The corpus reaches selections, ends and doubt.
    assert!(seen.turn_ids.contains("batched") && seen.turn_ids.contains(&"y".repeat(256)));
    assert!(seen.turn_ids.contains("edge") && seen.turn_ids.contains("late"));
    assert_eq!(seen.terminal.get("batched"), Some(&false));
    assert_eq!(seen.terminal.get("ontime"), Some(&true));
    assert!(!seen.terminal.contains_key("overdue"));
    assert!(seen.uncertain);
    assert!(!seen.turn_ids.contains("huge") && !seen.turn_ids.contains("nochannel"));
    // An empty channel names none.
    assert_eq!(
        request_activity(&records, &a, "", &[&c]),
        Selected::default()
    );
}
