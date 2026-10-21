use super::*;
use core::prelude::v1::test;
use serde_json::json;

const CHANNEL: &str = "0b8e2a3c-1d4f-4a5b-8c6d-7e8f9a0b1c2d";

fn bot() -> String {
    "c".repeat(64)
}

fn owner() -> String {
    "a".repeat(64)
}

fn event(id: &str, patch: impl FnOnce(&mut EventData)) -> Arc<EventData> {
    let mut event = EventData {
        id: format!("{id:0<64}"),
        pubkey: "b".repeat(64),
        created_at: 100,
        kind: 9,
        content: format!("hello {id}"),
        tags: vec![vec!["h".into(), CHANNEL.into()]],
    };
    patch(&mut event);
    Arc::new(event)
}

fn attention(objects: Value) -> BTreeMap<String, AttentionObject> {
    let record = AgentRecord::blank(&bot());
    let mut record = record;
    for (slug, value) in objects.as_object().unwrap() {
        record = set_attention(&record, slug, Some(value), 1, None).unwrap();
    }
    (*record.attention).clone()
}

fn kinds(triggers: &[Trigger]) -> Vec<&'static str> {
    triggers
        .iter()
        .map(|trigger| match trigger {
            Trigger::Mention { .. } => "mention",
            Trigger::Watch { .. } => "watch",
            Trigger::Timer { .. } => "timer",
        })
        .collect()
}

#[test]
fn the_queue_holds_32_and_admits_60_a_minute() {
    let start = Instant::now();
    let mut window = Window {
        start: None,
        admitted: 0,
    };
    for _ in 0..QUEUE_LIMIT {
        assert!(admit(&mut window, 0, start));
    }
    assert!(
        !admit(&mut window, QUEUE_LIMIT, start),
        "a full queue drops"
    );
    for _ in QUEUE_LIMIT..RATE_LIMIT as usize {
        assert!(admit(&mut window, 0, start));
    }
    assert!(!admit(&mut window, 0, start), "the 61st in a minute drops");
    assert!(
        !admit(&mut window, 0, start + Duration::from_secs(59)),
        "still the same minute"
    );
    assert!(
        admit(&mut window, 0, start + RATE_WINDOW),
        "a new minute admits again"
    );
    assert_eq!(window.admitted, 1);
}

#[test]
fn mentions_wake_and_addressed_events_skip_watches() {
    let agent = bot();
    let watches = compile(&attention(json!({
        "interest/default": {"type": "interest", "instructions": "Watch."},
        "watch/all": {"type": "event", "interest_id": "default", "enabled": true,
            "since": 1, "channels": "all", "kinds": []},
    })));
    let none = |_: &str| false;
    let mention = event("m", |e| e.tags.push(vec!["p".into(), agent.clone()]));
    assert_eq!(
        kinds(&heard(&agent, &owner(), &mention, &none, &watches)),
        ["mention"]
    );
    // A reaction to its message is addressed: no watch, and no run.
    let mine = |id: &str| id == "mine";
    let reaction = event("r", |e| {
        e.kind = 7;
        e.tags = vec![vec!["e".into(), "mine".into()]];
    });
    assert!(heard(&agent, &owner(), &reaction, &mine, &watches).is_empty());
    // A reply in a thread it started wakes it only by a watch, or by naming
    // it.
    let reply = |e: &mut EventData| {
        e.tags.push(vec![
            "e".into(),
            "mine".into(),
            String::new(),
            "reply".into(),
        ])
    };
    assert_eq!(
        kinds(&heard(
            &agent,
            &owner(),
            &event("q", reply),
            &mine,
            &watches
        )),
        ["watch"]
    );
    let named = event("n", |e| {
        reply(e);
        e.tags.push(vec!["p".into(), agent.clone()]);
    });
    assert_eq!(
        kinds(&heard(&agent, &owner(), &named, &mine, &watches)),
        ["mention"]
    );
    // Anything else reaches the watches, one run per match.
    let plain = event("p", |_| {});
    let triggers = heard(&agent, &owner(), &plain, &none, &watches);
    assert_eq!(kinds(&triggers), ["watch"]);
    let Trigger::Watch { slug, interest, .. } = &triggers[0] else {
        panic!("watch");
    };
    assert_eq!(slug, "watch/all");
    assert_eq!(interest.as_ref().unwrap().instructions, "Watch.");
}

#[test]
fn wakes_on_another_of_the_owners_agents_only_when_it_names_the_agent() {
    let agent = bot();
    let watches = compile(&attention(json!({
        "watch/all": {"type": "event", "interest_id": "default", "enabled": true,
            "since": 1, "channels": "all", "kinds": []},
    })));
    let mine = |id: &str| id == "mine";
    let auth = vec!["auth".to_string(), owner(), String::new(), "f".repeat(128)];
    let reply = |e: &mut EventData| {
        e.tags.push(vec![
            "e".into(),
            "mine".into(),
            String::new(),
            "reply".into(),
        ]);
    };
    // The sibling replies to its message without naming it, and says
    // what its watch matches. Neither wakes it: two agents replying to
    // each other would otherwise never stop.
    let unnamed = event("r", |e| {
        reply(e);
        e.tags.push(auth.clone());
    });
    assert!(heard(&agent, &owner(), &unnamed, &mine, &watches).is_empty());
    // Naming it wakes it, even in reply to its own message.
    let callback = event("c", |e| {
        reply(e);
        e.tags.push(vec!["p".into(), agent.clone()]);
        e.tags.push(auth.clone());
    });
    assert_eq!(
        kinds(&heard(&agent, &owner(), &callback, &mine, &watches)),
        ["mention"]
    );
}

#[test]
fn disabled_watches_and_bad_filters_never_match() {
    let watches = compile(&attention(json!({
        "watch/off": {"type": "event", "interest_id": "default", "enabled": false,
            "since": 1, "channels": "all", "kinds": []},
        "watch/go": {"type": "event", "interest_id": "default", "enabled": true,
            "since": 1, "channels": "all", "kinds": [9], "filter": "content == \"go\""},
    })));
    assert_eq!(watches.len(), 1);
    let none = |_: &str| false;
    assert!(heard(&bot(), &owner(), &event("x", |_| {}), &none, &watches).is_empty());
    let go = event("g", |e| e.content = "go".into());
    assert_eq!(
        kinds(&heard(&bot(), &owner(), &go, &none, &watches)),
        ["watch"]
    );
}

#[test]
fn due_timers_run_once_and_the_longest_waiting_first() {
    let objects = attention(json!({
        "watch/a": {"type": "timer", "interest_id": "default", "prompt": "a", "enabled": true,
            "interval_secs": 60, "armed_at": 1000, "max_occurrences": 2, "expires_at": null},
        "watch/b": {"type": "timer", "interest_id": "default", "prompt": "b", "enabled": true,
            "interval_secs": 30, "armed_at": 1000, "max_occurrences": null, "expires_at": null},
    }));
    let start = BTreeMap::from([
        (
            "watch/a".to_owned(),
            timer_state(
                match &objects["watch/a"].value {
                    AttentionValue::Timer(timer) => timer,
                    _ => unreachable!(),
                },
                None,
                1000,
            ),
        ),
        (
            "watch/b".to_owned(),
            TimerState {
                armed_at: 1000,
                next_due: 1030,
                used: 0,
            },
        ),
    ]);
    // Nothing is due before its first occurrence.
    let (_, none) = due_timer(&objects, &start, 1010);
    assert!(none.is_none());
    // b is due first (1030), then a (1060).
    let (timers, next) = due_timer(&objects, &start, 1061);
    assert_eq!(next.unwrap().0, "watch/b");
    assert_eq!(
        timers["watch/b"],
        TimerState {
            armed_at: 1000,
            next_due: 1091,
            used: 1
        },
        "one run when overdue, the next an interval after it"
    );
    let (timers, next) = due_timer(&objects, &timers, 1061);
    assert_eq!(next.unwrap().0, "watch/a");
    // Both overdue: b has waited longest, then a runs its second and last.
    let (timers, next) = due_timer(&objects, &timers, 1200);
    assert_eq!(next.unwrap().0, "watch/b");
    let (timers, next) = due_timer(&objects, &timers, 1200);
    assert_eq!(next.unwrap().0, "watch/a");
    assert_eq!(timers["watch/a"].used, 2);
    let (timers, next) = due_timer(&objects, &timers, 1200);
    assert!(next.is_none(), "one run each when overdue");
    let (_, next) = due_timer(&objects, &timers, 5000);
    assert_eq!(next.unwrap().0, "watch/b", "a is spent");
}

#[test]
fn a_timer_that_stops_after_one_run_runs_once() {
    let objects = attention(json!({
        "watch/t": {"type": "timer", "interest_id": "default", "prompt": "t", "enabled": true,
            "interval_secs": 60, "armed_at": 1000, "max_occurrences": 1, "expires_at": null},
    }));
    // Armed through the editor, which saves its state.
    let AttentionValue::Timer(timer) = &objects["watch/t"].value else {
        unreachable!()
    };
    let mut timers = BTreeMap::from([("watch/t".to_owned(), timer_state(timer, None, 1000))]);
    let mut runs = 0;
    for now in (1000..2000).step_by(5) {
        let (next, due) = due_timer(&objects, &timers, now);
        timers = next;
        runs += usize::from(due.is_some());
    }
    assert_eq!(runs, 1);
    assert_eq!(timers["watch/t"].used, 1);
}

#[test]
fn a_timer_without_state_does_not_run_for_what_it_missed() {
    let objects = attention(json!({
        "watch/t": {"type": "timer", "interest_id": "default", "prompt": "t", "enabled": true,
            "interval_secs": 10, "armed_at": 100, "max_occurrences": 1, "expires_at": null},
    }));
    // Saved state is gone: the occurrences due by now count as used.
    let (timers, next) = due_timer(&objects, &BTreeMap::new(), 135);
    assert!(next.is_none());
    assert_eq!(timers["watch/t"].used, 3);
}

/// The bot with `objects` as its attention, attention on or off.
fn agent_with(objects: &BTreeMap<String, AttentionObject>, on: bool) -> Agent {
    Agent {
        pubkey: bot(),
        name: "Echo".into(),
        agent_type: "p/kind".into(),
        owner: owner(),
        relay: "wss://relay.example.com".into(),
        picture: None,
        attention: Arc::new(objects.clone()),
        skipped: Arc::default(),
        timers: Arc::default(),
        attention_enabled: on,
        config: Value::Null,
    }
}

/// The job a watch match queues, as `dispatch` does.
fn queued(agent: &Agent, event: &Arc<EventData>, epoch: u64) -> Job {
    let watches = compile(&agent.attention);
    let trigger = heard(&agent.pubkey, &agent.owner, event, &|_| false, &watches)
        .pop()
        .expect("a watch matches");
    let Trigger::Watch { slug, .. } = &trigger else {
        panic!("a watch");
    };
    let matched = Some((agent.attention[slug].clone(), epoch));
    Job {
        trigger,
        channel: None,
        matched,
        work: None,
    }
}

#[test]
fn drops_a_queued_watch_run_whose_watch_or_attention_changed_before_it_ran() {
    let watch = json!({"type": "event", "interest_id": "default", "enabled": true,
        "since": 1, "channels": "all", "kinds": [9]});
    let objects = attention(json!({
        "interest/default": {"type": "interest", "instructions": "Look."},
        "watch/w": watch,
    }));
    let agent = agent_with(&objects, true);
    let hello = event("1", |_| {});
    assert!(rematched(0, &agent, queued(&agent, &hello, 0)).is_some());
    // Attention off, or off and on again since (a new epoch).
    assert!(rematched(0, &agent_with(&objects, false), queued(&agent, &hello, 0)).is_none());
    assert!(rematched(1, &agent, queued(&agent, &hello, 0)).is_none());
    // Its watch disabled, removed or replaced, even by the same value.
    let mut disabled = watch.clone();
    disabled["enabled"] = json!(false);
    let edited = |value: Option<Value>, at: u64| {
        let mut record = AgentRecord {
            attention: Arc::new(objects.clone()),
            ..AgentRecord::blank(&bot())
        };
        record = set_attention(&record, "watch/w", value.as_ref(), at, None).unwrap();
        agent_with(&record.attention, true)
    };
    for changed in [
        edited(Some(disabled), 2),
        edited(None, 2),
        edited(Some(watch.clone()), 2),
    ] {
        assert!(rematched(0, &changed, queued(&agent, &hello, 0)).is_none());
    }
    // A mention always runs.
    let mention = Job {
        trigger: Trigger::Mention { event: hello },
        channel: None,
        matched: None,
        work: None,
    };
    assert!(rematched(5, &agent_with(&objects, false), mention).is_some());
}

#[test]
fn tells_a_handed_off_run_whether_its_watch_or_timer_is_still_current() {
    let watch = json!({"type": "event", "interest_id": "default", "enabled": true,
        "since": 1, "channels": "all", "kinds": [9]});
    let timer = json!({"type": "timer", "interest_id": "default", "prompt": "tick",
        "enabled": true, "interval_secs": 60, "armed_at": 1000, "max_occurrences": null,
        "expires_at": null});
    let objects = attention(json!({
        "interest/default": {"type": "interest", "instructions": "Look."},
        "watch/w": watch,
        "watch/tick": timer,
    }));
    let agent = agent_with(&objects, true);
    let job = queued(
        &agent,
        &event("1", |event| event.content = "deploy".into()),
        0,
    );
    assert!(still_current(0, &agent, &job));
    // An earlier turn tightened the watch: its event no longer matches.
    let edit = |slug: &str, value: Value, at: u64| {
        let record = AgentRecord {
            attention: Arc::new(objects.clone()),
            ..AgentRecord::blank(&bot())
        };
        let record = set_attention(&record, slug, Some(&value), at, None).unwrap();
        agent_with(&record.attention, true)
    };
    let mut tightened = watch.clone();
    tightened["filter"] = json!("content == \"ship\"");
    assert!(!still_current(0, &edit("watch/w", tightened, 1), &job));
    assert!(!still_current(0, &agent_with(&objects, false), &job));
    // A timer occurrence is void once attention went off and on again, or
    // its timer was disabled or rearmed.
    let tick = Job {
        trigger: Trigger::Timer {
            slug: "watch/tick".into(),
            timer: serde_json::from_value(timer.clone()).unwrap(),
            interest: None,
            spent: false,
        },
        channel: None,
        work: None,
        matched: Some((objects["watch/tick"].clone(), 0)),
    };
    assert!(still_current(0, &agent, &tick));
    assert!(!still_current(1, &agent, &tick));
    assert!(!still_current(0, &agent_with(&objects, false), &tick));
    let mut rearmed = timer.clone();
    rearmed["armed_at"] = json!(1100);
    assert!(!still_current(0, &edit("watch/tick", rearmed, 1), &tick));
}

#[test]
fn type_defaults_become_the_first_record() {
    struct Kind;
    impl registry::AgentType for Kind {
        fn id(&self) -> &'static str {
            "kind"
        }
        fn title(&self) -> gpui_kit::SharedString {
            "Kind".into()
        }
        fn defaults(&self) -> registry::TypeDefaults {
            registry::TypeDefaults {
                config: json!({"model": "x"}),
                attention: [(
                    "interest/default".to_owned(),
                    AttentionValue::Interest(attention::Interest {
                        instructions: "Help.".into(),
                    }),
                )]
                .into(),
            }
        }
    }
    let kind = RegisteredType {
        key: "p/kind".into(),
        kind: Arc::new(Kind),
    };
    let record = defaults(&bot(), &kind).unwrap();
    assert_eq!(record.config, json!({"model": "x"}));
    assert!(record.attention.contains_key("interest/default"));
    // A new agent has no attention until its owner turns it on.
    assert!(!record.attention_on());
    assert!(registry::validate("p", &Kind).is_ok());
    assert!(registry::validate("P!", &Kind).is_err());
}

#[test]
fn only_the_holder_runs_an_agent_and_a_failed_claim_still_answers() {
    let bot = bot();
    assert!(claimed(&bot, Ok(AppAgentResponse::Claimed(true))));
    assert!(!claimed(&bot, Ok(AppAgentResponse::Claimed(false))));
    // A host from before claims, or one that cannot read its locks.
    assert!(claimed(&bot, Err(UNSUPPORTED.into())));
    assert!(claimed(&bot, Err("Could not read agent hosting".into())));
}

#[test]
fn a_claim_notices_a_changed_agent_or_type() {
    struct Kind;
    impl registry::AgentType for Kind {
        fn id(&self) -> &'static str {
            "kind"
        }
        fn title(&self) -> gpui_kit::SharedString {
            "Kind".into()
        }
        fn defaults(&self) -> registry::TypeDefaults {
            registry::TypeDefaults::default()
        }
    }
    let kind = || RegisteredType {
        key: "p/kind".into(),
        kind: Arc::new(Kind),
    };
    let agent = Agent {
        pubkey: bot(),
        name: "Echo".into(),
        agent_type: "p/kind".into(),
        owner: "a".repeat(64),
        relay: "wss://relay.example.com".into(),
        picture: None,
        attention: Arc::default(),
        skipped: Arc::default(),
        timers: Arc::default(),
        attention_enabled: true,
        config: Value::Null,
    };
    let first = kind();
    let seen = Seen {
        agent: Some(agent.clone()),
        kind: Some(first.clone()),
    };
    assert!(
        seen == Seen {
            agent: Some(agent.clone()),
            kind: Some(first),
        }
    );
    // The same type registered again is a new type.
    assert!(
        seen != Seen {
            agent: Some(agent.clone()),
            kind: Some(kind()),
        }
    );
    let renamed = Agent {
        name: "Echo 2".into(),
        ..agent
    };
    assert!(
        seen != Seen {
            agent: Some(renamed),
            kind: seen.kind.clone(),
        }
    );
    assert!(
        seen != Seen {
            agent: None,
            kind: None
        }
    );
}

const HERE: &str = "https://relay.example.test";
const OTHER: &str = "https://other.example.test";

fn viewer() -> String {
    "a".repeat(64)
}

/// Both communities bound, `HERE` and `OTHER`, each ready for the viewer.
fn both_ready() -> Sessions {
    Sessions::from([
        (HERE.to_owned(), Some(viewer())),
        (OTHER.to_owned(), Some(viewer())),
    ])
}

#[test]
fn keeps_running_an_agent_while_another_community_is_selected_and_gives_it_only_its_communitys_events()
 {
    let (owner, relay) = (viewer(), "wss://relay.example.test");
    let sessions = both_ready();
    let viewer = Some(owner.as_str());
    assert!(shown(&owner, relay, Some(HERE), &sessions));
    // Another community selected: no longer shown, still running.
    assert!(!shown(&owner, relay, Some(OTHER), &sessions));
    assert!(!shown(&owner, relay, None, &sessions));
    assert!(alive(&owner, relay, viewer, &sessions));
    assert!(ready(&owner, relay, &sessions));
    // It hears its own community, never the selected one's.
    assert!(hears(&owner, relay, HERE, &sessions));
    assert!(!hears(&owner, relay, OTHER, &sessions));
}

#[test]
fn connects_an_agents_community_at_start_without_selecting_it() {
    let viewer = viewer();
    let stranger = "d".repeat(64);
    let identities = [
        (viewer.as_str(), "wss://other.example.test"),
        (stranger.as_str(), "wss://third.example.test"),
    ];
    assert_eq!(
        wanted(Some(&viewer), Some(HERE), identities),
        BTreeSet::from([HERE.to_owned(), OTHER.to_owned()]),
        "the selected community and the viewer's agent's, not another owner's"
    );
    assert_eq!(
        wanted(Some(&viewer), None, identities),
        BTreeSet::from([OTHER.to_owned()])
    );
    assert!(
        wanted(None, Some(HERE), identities).is_empty(),
        "signed out"
    );
}

#[test]
fn keeps_an_agent_running_while_its_community_reconnects_and_stops_it_on_leave() {
    let (owner, relay) = (viewer(), "wss://relay.example.test");
    let viewer = Some(owner.as_str());
    let mut sessions = both_ready();
    // Reconnecting: the session is not ready, so it is not shown and hears
    // nothing, but keeps running.
    sessions.insert(HERE.to_owned(), None);
    assert!(alive(&owner, relay, viewer, &sessions));
    assert!(!ready(&owner, relay, &sessions));
    assert!(!shown(&owner, relay, Some(HERE), &sessions));
    assert!(!hears(&owner, relay, HERE, &sessions));
    sessions.insert(HERE.to_owned(), Some(owner.clone()));
    assert!(ready(&owner, relay, &sessions));
    // Signed out, or left: it stops.
    assert!(!alive(&owner, relay, None, &sessions));
    sessions.remove(HERE);
    assert!(!alive(&owner, relay, viewer, &sessions));
    // Another owner's agent never runs here.
    let stranger = "d".repeat(64);
    assert!(!alive(
        &stranger,
        "wss://other.example.test",
        viewer,
        &sessions
    ));
    assert!(!ready(&stranger, "wss://other.example.test", &sessions));
}

#[test]
fn tries_a_failed_unselected_community_again_every_so_often_and_leaves_the_selected_ones_retry_to_the_viewer()
 {
    let start = Instant::now();
    let at = |secs: u64| start + Duration::from_secs(secs);
    // Failed while selected: the viewer has its Retry.
    assert!(!retry_due(true, true, None, at(10)));
    // Another community selected: tried at the next tick.
    assert!(retry_due(false, true, None, at(15)));
    let retried = Some(at(15));
    assert!(!retry_due(false, true, retried, at(35)));
    assert!(retry_due(false, true, retried, at(45)));
    // A connected one is left alone.
    assert!(!retry_due(false, false, None, at(15)));
    assert_eq!(RETRY, Duration::from_secs(30));
}

#[test]
fn passes_the_model_and_effort_its_owner_asked_for_and_no_other() {
    use super::registry::{Effort, Model};
    let viewer = "a".repeat(64);
    let sibling = "d".repeat(64);
    let level = |id: &str| Effort {
        id: id.into(),
        label: id.into(),
    };
    let offered = Models {
        current: "fast".into(),
        models: vec![
            Model {
                id: "fast".into(),
                label: "Fast".into(),
                efforts: vec![level("low"), level("high")],
                effort: None,
            },
            Model {
                id: "deep".into(),
                label: "Deep".into(),
                efforts: vec![level("max")],
                effort: None,
            },
        ],
    };
    let asked_by = |pubkey: &str, extra: &[&[&str]]| {
        let event = event("m", |event| {
            event.pubkey = pubkey.into();
            for tag in extra {
                event
                    .tags
                    .push(tag.iter().map(|part| part.to_string()).collect());
            }
        });
        let trigger = Trigger::Mention { event };
        asked(&trigger, &bot(), &viewer).map(|asked| picked(asked, Some(&offered)))
    };
    let some = |model: Option<&str>, effort: Option<&str>| {
        Some((model.map(str::to_string), effort.map(str::to_string)))
    };
    assert_eq!(
        asked_by(&viewer, &[&["effort", &bot(), "high"]]),
        some(None, Some("high"))
    );
    // Nobody else picks, and a pick for another agent is not this one's.
    assert_eq!(
        asked_by(
            &"b".repeat(64),
            &[&["effort", &bot(), "high"], &["model", &bot(), "deep"]]
        ),
        None
    );
    assert_eq!(asked_by(&viewer, &[&["effort", &sibling, "high"]]), None);
    assert_eq!(
        asked_by(&viewer, &[&["effort", &bot(), "max"]]),
        some(None, None)
    );
    // An effort counts only for the model the run uses.
    assert_eq!(
        asked_by(
            &viewer,
            &[&["model", &bot(), "deep"], &["effort", &bot(), "max"]]
        ),
        some(Some("deep"), Some("max"))
    );
    assert_eq!(
        asked_by(
            &viewer,
            &[&["model", &bot(), "deep"], &["effort", &bot(), "low"]]
        ),
        some(Some("deep"), None)
    );
    assert_eq!(
        asked_by(
            &viewer,
            &[&["model", &bot(), "gone"], &["effort", &bot(), "low"]]
        ),
        some(None, Some("low"))
    );
    // A type that offers nothing gets nothing.
    assert_eq!(
        picked((Some("deep".into()), Some("max".into())), None),
        (None, None)
    );
}

#[test]
fn a_type_changes_only_between_turns_and_where_the_agent_runs() {
    // Not while a turn, or the work it handed off, is still going.
    assert_eq!(
        retype_refusal("Echo", 1, true, false).as_deref(),
        Some("Echo is answering; change its type when it finishes")
    );
    assert_eq!(
        retype_refusal("Echo", 0, false, false).as_deref(),
        Some("Change its type on the device that runs it")
    );
    assert_eq!(
        retype_refusal("Echo", 0, true, true).as_deref(),
        Some("Echo's provider is already changing")
    );
    assert_eq!(retype_refusal("Echo", 0, true, false), None);
}

#[test]
fn a_run_counts_as_working_until_its_handed_off_work_settles() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let working = Working::none(runtime.handle().clone()).counted(count.clone());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let (done, handed) = futures::channel::oneshot::channel::<()>();
    working.until(async move {
        let _ = handed.await;
        WorkOutcome::ok()
    });
    working.release();
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "handed-off work still runs"
    );
    done.send(()).unwrap();
    for _ in 0..100 {
        if count.load(Ordering::SeqCst) == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
