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
