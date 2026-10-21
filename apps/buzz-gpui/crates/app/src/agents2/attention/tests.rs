use super::*;
use core::prelude::v1::test;
use serde_json::json;

const CHANNEL: &str = "0b8e2a3c-1d4f-4a5b-8c6d-7e8f9a0b1c2d";

fn agent() -> String {
    "a".repeat(64)
}

fn event() -> EventData {
    EventData {
        id: "e".repeat(64),
        pubkey: "b".repeat(64),
        created_at: 100,
        kind: 9,
        content: "hi".into(),
        tags: vec![vec!["h".into(), CHANNEL.into()]],
    }
}

fn tags(tags: &[&[&str]]) -> Vec<Vec<String>> {
    tags.iter()
        .map(|tag| tag.iter().map(|s| s.to_string()).collect())
        .collect()
}

fn watch_json() -> Value {
    json!({
        "type": "event",
        "interest_id": "default",
        "enabled": true,
        "since": 50,
        "channels": [CHANNEL],
        "kinds": [9],
    })
}

fn watch() -> EventWatch {
    match AttentionValue::parse("watch/w", &watch_json()).unwrap() {
        AttentionValue::Event(watch) => watch,
        other => panic!("{other:?}"),
    }
}

fn with(mut value: Value, key: &str, field: Value) -> Value {
    value[key] = field;
    value
}

#[test]
fn accepts_each_spec_value_at_a_matching_slug() {
    assert_eq!(
        validate_object(
            "interest/default",
            &json!({"type": "interest", "instructions": "x"})
        ),
        None
    );
    assert_eq!(validate_object("watch/w", &watch_json()), None);
    assert_eq!(
        validate_object(
            "watch/t",
            &json!({
                "type": "timer", "interest_id": "default", "prompt": "tick",
                "enabled": true, "interval_secs": 60, "armed_at": 0,
                "max_occurrences": null, "expires_at": null,
            })
        ),
        None
    );
}

fn problem(slug: &str, value: &Value) -> String {
    validate_object(slug, value).expect("refused")
}

#[test]
fn refuses_wrong_slugs_unknown_or_missing_fields_and_bad_values() {
    assert!(problem("other/x", &watch_json()).contains("slug"));
    assert!(problem("interest/x", &watch_json()).contains("interest"));
    assert!(problem("watch/w", &with(watch_json(), "extra", json!(1))).contains("Unknown"));
    let mut missing = watch_json();
    missing.as_object_mut().unwrap().remove("since");
    assert!(problem("watch/w", &missing).contains("Missing"));
    assert!(
        problem("watch/w", &with(watch_json(), "channels", json!(["c1"]))).contains("channels")
    );
    assert!(
        problem("watch/w", &with(watch_json(), "tags", json!({"h": ["x"]})))
            .contains("other than h")
    );
    assert!(
        problem("watch/w", &with(watch_json(), "filter", json!("author =="))).contains("string")
    );
    assert!(problem("watch/w", &with(watch_json(), "since", json!(0))).contains("since"));
    assert!(problem("watch/w", &with(watch_json(), "enabled", json!("yes"))).contains("enabled"));
    assert!(problem("watch/w", &with(watch_json(), "name", json!("a\nb"))).contains("single line"));
    assert!(problem("watch/bad id", &watch_json()).contains("slug"));
}

#[test]
fn refuses_values_over_the_spec_limits() {
    assert!(
        problem(
            "interest/x",
            &json!({"type": "interest", "instructions": "x".repeat(16_385)})
        )
        .contains("16,384 bytes")
    );
    let many = vec!["true"; 129].join(" && ");
    assert!(problem("watch/w", &with(watch_json(), "filter", json!(many))).contains("256 parts"));
    assert!(
        problem(
            "watch/w",
            &with(watch_json(), "filter", json!("x".repeat(4_097)))
        )
        .contains("4,096 bytes")
    );
    let kinds: Vec<u16> = (0..101).collect();
    assert!(problem("watch/w", &with(watch_json(), "kinds", json!(kinds))).contains("kinds"));
    let classified = |question: Value| {
        validate_object(
            "watch/w",
            &with(
                watch_json(),
                "classifier",
                json!({"questions": {"a": question}}),
            ),
        )
    };
    // As in Janet, only the question is required.
    assert_eq!(classified(json!({"question": "q"})), None);
    assert!(
        classified(json!({}))
            .unwrap()
            .contains("Missing field: question")
    );
    assert!(
        classified(json!({"question": "q", "threshold": 1}))
            .unwrap()
            .contains("threshold")
    );
    assert_eq!(
        validate_object(
            "watch/w",
            &with(
                watch_json(),
                "classifier",
                json!({"questions": {"a": {"question": "q", "threshold": 0.5, "true": "y", "false": "n"}}})
            )
        ),
        None
    );
}

#[test]
fn filters_evaluate_with_precedence_and_negation() {
    let author = "c".repeat(64);
    let filter = compile_filter(&format!(
        r#"!is_reply && (content == "go" || author == "{author}")"#
    ))
    .unwrap();
    let go = EventData {
        content: "go".into(),
        ..event()
    };
    assert!(filter(&go));
    assert!(filter(&EventData {
        pubkey: author,
        ..event()
    }));
    assert!(!filter(&event()));
    assert!(!filter(&EventData {
        tags: tags(&[&["e", "x", "", "reply"]]),
        ..go
    }));
    // A mention marker is not a reply.
    let mention = compile_filter("is_reply").unwrap();
    assert!(!mention(&EventData {
        tags: tags(&[&["e", "x", "", "mention"]]),
        ..event()
    }));
    let escaped = compile_filter(r#"content == "say \"hi\"""#).unwrap();
    assert!(escaped(&EventData {
        content: "say \"hi\"".into(),
        ..event()
    }));
}

#[test]
fn filters_refuse_trailing_text_and_unknown_words() {
    let error = |source: &str| compile_filter(source).err().expect("refused");
    assert!(error("true false").contains("end"));
    assert!(error("trueish").contains("Unexpected"));
    assert!(error("(true").contains(")"));
    assert!(error("content = \"x\"").contains("=="));
    assert!(error("content == \"x").contains("string"));
    let deep = format!("{}true{}", "(".repeat(40), ")".repeat(40));
    assert!(error(&deep).contains("nested"));
}

#[test]
fn watches_match_by_since_channel_kind_and_tags() {
    let watch = watch();
    assert!(watch_matches(&watch, &event(), None));
    let disabled = EventWatch {
        enabled: false,
        ..watch.clone()
    };
    assert!(!watch_matches(&disabled, &event(), None));
    assert!(!watch_matches(
        &watch,
        &EventData {
            created_at: 10,
            ..event()
        },
        None
    ));
    assert!(!watch_matches(
        &watch,
        &EventData {
            tags: vec![],
            ..event()
        },
        None
    ));
    assert!(!watch_matches(
        &watch,
        &EventData { kind: 7, ..event() },
        None
    ));
    let tagged = EventWatch {
        tags: Some([("t".to_owned(), vec!["x".to_owned()])].into()),
        ..watch.clone()
    };
    assert!(!watch_matches(&tagged, &event(), None));
    let all = EventWatch {
        channels: Channels::ALL,
        kinds: vec![],
        ..watch
    };
    assert!(watch_matches(
        &all,
        &EventData {
            kind: 7,
            tags: vec![],
            ..event()
        },
        None
    ));
    let never = compile_filter("false").unwrap();
    assert!(!watch_matches(&all, &event(), Some(&never)));
}

#[test]
fn mentions_dms_and_reactions_to_the_agent_not_replies_are_addressed() {
    let agent = agent();
    let none = |_: &str| false;
    let mine = |id: &str| id == "mine";
    let p = tags(&[&["p", &agent]]);
    assert!(addressed_to(
        &EventData {
            tags: p.clone(),
            ..event()
        },
        &agent,
        none
    ));
    assert!(!addressed_to(
        &EventData {
            kind: 1,
            tags: p.clone(),
            ..event()
        },
        &agent,
        none
    ));
    // A reply in its thread is not addressed unless it names the agent;
    // only a watch can wake it for one.
    assert!(!addressed_to(
        &EventData {
            tags: tags(&[&["e", "mine", "", "reply"]]),
            ..event()
        },
        &agent,
        mine
    ));
    assert!(!addressed_to(
        &EventData {
            pubkey: agent.clone(),
            tags: p.clone(),
            ..event()
        },
        &agent,
        none
    ));
    assert!(addressed_to(
        &EventData {
            kind: 4,
            tags: p.clone(),
            ..event()
        },
        &agent,
        none
    ));
    assert!(addressed_to(
        &EventData {
            kind: 7,
            tags: tags(&[&["e", "mine"]]),
            ..event()
        },
        &agent,
        mine
    ));
    assert!(!addressed_to(
        &EventData {
            kind: 7,
            tags: p,
            ..event()
        },
        &agent,
        none
    ));
}

#[test]
fn timers_count_what_was_already_due_and_are_spent() {
    let timer = TimerWatch {
        interest_id: "default".into(),
        prompt: "tick".into(),
        enabled: true,
        interval_secs: 10,
        armed_at: 100,
        max_occurrences: Some(1),
        expires_at: None,
    };
    let state = timer_state(&timer, None, 100);
    assert_eq!(
        state,
        TimerState {
            armed_at: 100,
            next_due: 110,
            used: 0
        }
    );
    let used = TimerState { used: 1, ..state };
    assert_eq!(timer_state(&timer, Some(&used), 500).used, 1);
    // No saved state at 135: occurrences at 110, 120 and 130 count as used.
    assert_eq!(
        timer_state(&timer, None, 135),
        TimerState {
            armed_at: 100,
            next_due: 140,
            used: 3
        }
    );
    let rearmed = TimerWatch {
        armed_at: 200,
        ..timer.clone()
    };
    assert_eq!(timer_state(&rearmed, Some(&state), 200).next_due, 210);
    assert!(timer_spent(&timer, &used, 0));
    let expiring = TimerWatch {
        expires_at: Some(50),
        ..timer
    };
    assert!(timer_spent(&expiring, &state, 50));
}

#[test]
fn values_read_and_write_in_the_wire_shape() {
    let value = AttentionValue::parse("watch/w", &watch_json()).unwrap();
    assert_eq!(value.to_json(), watch_json());
    let all = with(watch_json(), "channels", json!("all"));
    let AttentionValue::Event(read) = AttentionValue::parse("watch/w", &all).unwrap() else {
        panic!("event");
    };
    assert!(read.channels.is_all());
    assert_eq!(AttentionValue::Event(read).to_json(), all);
    let timer = json!({
        "type": "timer", "interest_id": "default", "prompt": "tick",
        "enabled": true, "interval_secs": 60.0, "armed_at": 0,
        "max_occurrences": null, "expires_at": null,
    });
    assert!(
        AttentionValue::parse("watch/t", &timer).is_ok(),
        "60.0 is whole"
    );
}

#[test]
fn watches_read_as_one_line() {
    let channels = [ChannelChoice {
        id: CHANNEL.into(),
        name: "general".into(),
    }];
    let value = AttentionValue::Event(watch());
    assert_eq!(describe_watch(&value, &channels), "Messages in #general");
    assert_eq!(describe_watch(&value, &[]), "Messages in 1 channel");
    let any = AttentionValue::Event(EventWatch {
        channels: Channels::ALL,
        kinds: vec![],
        filter: Some("true".into()),
        ..watch()
    });
    assert_eq!(
        describe_watch(&any, &[]),
        "Any event in any channel, filtered"
    );
    let kinds = AttentionValue::Event(EventWatch {
        kinds: vec![7, 9],
        channels: Channels::Some(vec![CHANNEL.into(), CHANNEL.replace('0', "1")]),
        ..watch()
    });
    assert_eq!(
        describe_watch(&kinds, &[]),
        "Kind 7, 9 events in 2 channels"
    );
    assert_eq!(every(60), "Every minute");
    assert_eq!(every(7_200), "Every 2 hours");
    assert_eq!(every(90), "Every 90 seconds");
    assert_eq!(interest_title("release-triage"), "Release triage");
    assert_eq!(interest_title("--"), "--");
    assert_eq!(plural(2, "match"), "2 matches");
    assert_eq!(grouped(65_535), "65,535");
}
