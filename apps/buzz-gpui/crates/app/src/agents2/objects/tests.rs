use super::*;
use crate::agents2::store::SkippedObject;
use core::prelude::v1::test;
use std::collections::BTreeMap;
use std::sync::Arc;

const CHANNEL: &str = "0b8e2a3c-1d4f-4a5b-8c6d-7e8f9a0b1c2d";
const NOW: u64 = 10_000;

fn interest(instructions: &str) -> Value {
    json!({"type": "interest", "instructions": instructions})
}

fn watch(patch: Value) -> Value {
    let mut value = json!({
        "type": "event", "interest_id": "triage", "enabled": true, "since": 1,
        "channels": [CHANNEL], "kinds": [9],
    });
    for (key, field) in patch.as_object().unwrap() {
        value[key] = field.clone();
    }
    value
}

fn timer(patch: Value) -> Value {
    let mut value = json!({
        "type": "timer", "interest_id": "triage", "prompt": "check", "enabled": true,
        "interval_secs": 60, "armed_at": NOW, "max_occurrences": 1, "expires_at": null,
    });
    for (key, field) in patch.as_object().unwrap() {
        value[key] = field.clone();
    }
    value
}

fn set(record: &AgentRecord, slug: &str, value: Value) -> AgentRecord {
    set_attention(record, slug, Some(&value), NOW, None).unwrap()
}

fn record() -> AgentRecord {
    let value = AgentRecord::blank(&"a".repeat(64));
    let value = set(&value, "interest/triage", interest("Triage."));
    let value = set(&value, "interest/other", interest("Other."));
    set(&value, "watch/asks", watch(json!({"name": "Asks"})))
}

/// A write with the token of the latest show.
fn write(
    value: &AgentRecord,
    slug: &str,
    next: Option<Value>,
    scope: Option<&str>,
) -> Result<AgentRecord, AttentionError> {
    let expected = show(value, slug, NOW, None).unwrap().expected_state;
    checked_write(
        value,
        slug,
        next.as_ref(),
        &WriteOptions {
            expected,
            interest: scope.map(str::to_owned),
            rearm: false,
        },
        NOW,
    )
}

fn refused(result: Result<AgentRecord, AttentionError>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn tokens_are_absent_for_a_missing_object_and_follow_content() {
    let shown = show(&record(), "watch/none", NOW, None).unwrap();
    assert_eq!(shown.object, None);
    assert_eq!(shown.expected_state, ABSENT);
    assert_eq!(
        shown.to_json(),
        json!({"result": "not-found", "object": null, "expected_state": ABSENT})
    );
    let object = |instructions: &str| ShownObject::Interest {
        id: "x".into(),
        interest: Interest {
            instructions: instructions.into(),
        },
    };
    let a = token(Some(&object("i")));
    assert!(a.starts_with("object-v1:") && a.len() == "object-v1:".len() + 64);
    assert_eq!(a, token(Some(&object("i"))));
    assert_ne!(a, token(Some(&object("j"))));
}

#[test]
fn refuses_a_write_without_a_token_with_a_stale_one_or_creating_over_an_object() {
    let start = record();
    let shown = show(&start, "watch/asks", NOW, None).unwrap();
    let options = |expected: &str| WriteOptions {
        expected: expected.into(),
        ..Default::default()
    };
    assert!(
        refused(checked_write(&start, "watch/asks", None, &options(""), NOW))
            .starts_with("invalid-operation:")
    );
    let edited = write(
        &start,
        "watch/asks",
        Some(watch(json!({"name": "Edited"}))),
        None,
    )
    .unwrap();
    assert!(
        refused(checked_write(
            &edited,
            "watch/asks",
            None,
            &options(&shown.expected_state),
            NOW
        ))
        .starts_with("conflict:")
    );
    assert!(
        refused(checked_write(
            &edited,
            "watch/asks",
            Some(&watch(json!({}))),
            &options(ABSENT),
            NOW
        ))
        .starts_with("conflict:")
    );
    // An unrelated object's edit does not conflict.
    let other = write(&edited, "interest/other", Some(interest("Changed.")), None).unwrap();
    assert_eq!(
        show(&other, "watch/asks", NOW, None)
            .unwrap()
            .expected_state,
        show(&edited, "watch/asks", NOW, None)
            .unwrap()
            .expected_state
    );
}

#[test]
fn keeps_a_scoped_caller_to_its_own_interest() {
    let start = record();
    assert!(
        show(&start, "watch/asks", NOW, Some("other"))
            .unwrap_err()
            .to_string()
            .starts_with("wrong-scope:")
    );
    let other = watch(json!({"interest_id": "other"}));
    assert!(
        refused(write(&start, "watch/new", Some(other), Some("triage")))
            .starts_with("wrong-scope:")
    );
    assert!(
        refused(write(
            &start,
            "interest/other",
            Some(interest("x")),
            Some("triage")
        ))
        .starts_with("wrong-scope:")
    );
    let added = write(&start, "watch/new", Some(watch(json!({}))), Some("triage")).unwrap();
    assert!(added.attention.contains_key("watch/new"));
}

#[test]
fn needs_the_watch_interest_and_never_moves_or_retypes_a_watch() {
    let start = record();
    let lost = refused(write(
        &start,
        "watch/lost",
        Some(watch(json!({"interest_id": "missing"}))),
        None,
    ));
    assert!(
        lost.starts_with("not-found:") && lost.contains("set it first"),
        "{lost}"
    );
    let moved = refused(write(
        &start,
        "watch/asks",
        Some(watch(json!({"interest_id": "other"}))),
        None,
    ));
    assert!(
        moved.starts_with("invalid-operation:") && moved.contains("move"),
        "{moved}"
    );
    let retyped = refused(write(&start, "watch/asks", Some(timer(json!({}))), None));
    assert!(
        retyped.starts_with("invalid-operation:") && retyped.contains("type"),
        "{retyped}"
    );
    assert!(
        refused(write(&start, "watch/asks", Some(interest("x")), None)).starts_with("invalid:")
    );
}

#[test]
fn removes_an_interest_only_after_its_watches() {
    let start = record();
    let used = refused(write(&start, "interest/triage", None, None));
    assert!(
        used.starts_with("invalid-operation:") && used.contains("asks"),
        "{used}"
    );
    let bare = write(&start, "watch/asks", None, None).unwrap();
    let gone = write(&bare, "interest/triage", None, None).unwrap();
    assert_eq!(interest_ids(&gone, ""), vec!["other".to_string()]);
}

#[test]
fn reports_the_format_problems_as_invalid() {
    assert!(
        refused(write(
            &record(),
            "watch/asks",
            Some(watch(json!({"since": 0}))),
            None
        ))
        .starts_with("invalid: since")
    );
    // Before any scope or Interest check could name a field it lacks.
    assert!(
        refused(write(
            &record(),
            "watch/junk",
            Some(json!("junk")),
            Some("triage")
        ))
        .starts_with("invalid:")
    );
}

#[test]
fn keeps_agent_timers_a_minute_apart_except_one_the_owner_set() {
    assert!(
        refused(write(
            &record(),
            "watch/fast",
            Some(timer(json!({"interval_secs": 1}))),
            None
        ))
        .starts_with("invalid: interval_secs must be at least 60")
    );
    let owned = set(&record(), "watch/fast", timer(json!({"interval_secs": 1})));
    let edited = write(
        &owned,
        "watch/fast",
        Some(timer(json!({"interval_secs": 1, "prompt": "faster"}))),
        None,
    )
    .unwrap();
    let AttentionValue::Timer(timer) = &edited.attention["watch/fast"].value else {
        panic!("a timer");
    };
    assert_eq!(timer.prompt, "faster");
}

#[test]
fn caps_the_attention_one_agent_can_store() {
    let mut value = record();
    let long = "x".repeat(16_000);
    for index in 0..16 {
        value = write(
            &value,
            &format!("interest/i{index}"),
            Some(interest(&long)),
            None,
        )
        .unwrap();
    }
    assert!(
        refused(write(&value, "interest/more", Some(interest(&long)), None))
            .starts_with("invalid: Your attention would take more than 256 KB")
    );
    // Removing is always allowed.
    let value = write(&value, "interest/i0", None, None).unwrap();
    assert!(!value.attention.contains_key("interest/i0"));
}

#[test]
fn refuses_to_replace_an_object_this_app_could_not_read() {
    let value = AgentRecord {
        skipped: Arc::new(BTreeMap::from([(
            "watch/new".to_string(),
            SkippedObject {
                slug: "watch/new".into(),
                value: json!({"type": "future"}),
                modified_at: 1,
                problem: "Unknown type".into(),
            },
        )])),
        ..record()
    };
    for next in [Some(watch(json!({}))), None] {
        assert!(
            refused(write(&value, "watch/new", next, None)).starts_with(
                "invalid-operation: watch/new is stored in a form this app cannot read"
            )
        );
    }
}

#[test]
fn summaries_give_each_watch_status_and_filter() {
    let mut value = record();
    value = write(&value, "watch/once", Some(timer(json!({}))), None).unwrap();
    value = write(
        &value,
        "watch/off",
        Some(timer(json!({"enabled": false}))),
        None,
    )
    .unwrap();
    value = write(
        &value,
        "watch/done",
        Some(timer(json!({"armed_at": NOW - 600, "max_occurrences": 2}))),
        None,
    )
    .unwrap();
    value = write(
        &value,
        "watch/judged",
        Some(watch(json!({
            "enabled": false,
            "classifier": {"questions": {"a": {"question": "q"}}},
        }))),
        None,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(watch_summaries(&value, &WatchFilter::default(), NOW)).unwrap(),
        json!([
            {"id": "asks", "interest_id": "triage", "name": "Asks", "type": "event", "status": "enabled"},
            {"id": "done", "interest_id": "triage", "type": "timer", "status": "spent"},
            {"id": "judged", "interest_id": "triage", "type": "event", "status": "disabled", "classified": true},
            {"id": "off", "interest_id": "triage", "type": "timer", "status": "inactive"},
            {"id": "once", "interest_id": "triage", "type": "timer", "status": "armed"},
        ])
    );
    let ids = |filter: WatchFilter| -> Vec<String> {
        watch_summaries(&value, &filter, NOW)
            .into_iter()
            .map(|summary| summary.id)
            .collect()
    };
    assert_eq!(
        ids(WatchFilter {
            kind: Some("timer".into()),
            status: Some(WatchStatus::Spent),
            ..Default::default()
        }),
        ["done"]
    );
    assert_eq!(
        ids(WatchFilter {
            search: Some("As".into()),
            ..Default::default()
        }),
        ["asks"]
    );
    assert_eq!(
        ids(WatchFilter {
            search: Some("as".into()),
            ..Default::default()
        }),
        ["asks"]
    );
    assert!(
        ids(WatchFilter {
            interest: Some("other".into()),
            ..Default::default()
        })
        .is_empty()
    );
    let once = show(&value, "watch/once", NOW, None).unwrap().to_json();
    assert_eq!(once["object"]["status"], "armed");
    assert_eq!(once["object"]["used"], 0);
    assert_eq!(once["object"]["next_due"], NOW + 60);
    assert_eq!(once["object"]["id"], "once");
}
