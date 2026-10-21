use super::*;
use core::prelude::v1::test;
use serde_json::json;

fn pubkey() -> String {
    "a".repeat(64)
}

fn record() -> AgentRecord {
    AgentRecord {
        config: json!({"word": "hi"}),
        ..AgentRecord::blank(&pubkey())
    }
}

fn stored(storage: &MemoryStorage) -> Value {
    serde_json::from_str(&storage.read().unwrap()).unwrap()
}

#[test]
fn round_trips_records_and_keeps_objects_a_reader_would_ignore() {
    let storage = MemoryStorage::default();
    let saved = set_attention(
        &record(),
        "interest/default",
        Some(&json!({"type": "interest", "instructions": "Be brief."})),
        7,
        None,
    )
    .unwrap();
    let records = BTreeMap::from([(pubkey(), saved.clone())]);
    write_records(&storage, &records).unwrap();
    assert_eq!(read_records(&storage), records);

    let mut raw = stored(&storage);
    raw["agents"][pubkey()]["attention"]["watch/bad"] =
        json!({"slug": "watch/bad", "value": {"type": "event"}, "modifiedAt": 1});
    raw["agents"]["nonsense"] = json!({"pubkey": "nonsense", "attention": {}});
    storage.write(&raw.to_string()).unwrap();
    let read = read_records(&storage);
    assert_eq!(read[&pubkey()].attention, saved.attention);
    assert!(!read.contains_key("nonsense"));
    // The invalid object is reported, and written back as it was.
    let bad = &read[&pubkey()].skipped["watch/bad"];
    assert_eq!(bad.value, json!({"type": "event"}));
    assert!(bad.problem.contains("Missing"), "{}", bad.problem);
    write_records(&storage, &read).unwrap();
    assert_eq!(
        stored(&storage)["agents"][pubkey()]["attention"]["watch/bad"],
        json!({"slug": "watch/bad", "value": {"type": "event"}, "modifiedAt": 1})
    );
    // Removing it clears it from the skipped set.
    let fixed = set_attention(&read[&pubkey()], "watch/bad", None, 2, None).unwrap();
    assert!(fixed.skipped.is_empty());
}

#[test]
fn skips_objects_over_the_count_limit_in_slug_order_and_refuses_more() {
    let storage = MemoryStorage::default();
    let attention: Map<String, Value> = (0..101)
        .map(|n| {
            let slug = format!("interest/i{n:03}");
            (
                slug.clone(),
                json!({"slug": slug, "value": {"type": "interest", "instructions": "x"}, "modifiedAt": 1}),
            )
        })
        .collect();
    storage
        .write(
            &json!({"version": 1, "agents": {pubkey(): {"pubkey": pubkey(), "attention": attention}}})
                .to_string(),
        )
        .unwrap();
    let read = read_records(&storage).remove(&pubkey()).unwrap();
    assert_eq!(read.attention.len(), 100);
    assert_eq!(
        read.skipped.keys().collect::<Vec<_>>(),
        vec!["interest/i100"]
    );
    let interest = json!({"type": "interest", "instructions": "x"});
    let error = set_attention(&read, "interest/new", Some(&interest), 1, None).unwrap_err();
    assert!(error.contains("at most 100"), "{error}");
    let mut full = (*read.attention).clone();
    full.remove("interest/i099");
    let room = AgentRecord {
        attention: Arc::new(full),
        ..read.clone()
    };
    assert!(set_attention(&room, "interest/new", Some(&interest), 1, None).is_ok());
    // Replacing an existing object is not adding one.
    assert!(set_attention(&read, "interest/i000", Some(&interest), 1, None).is_ok());
}

#[test]
fn keeps_a_timer_budget_across_edits_and_only_a_rearm_moves_its_deadline() {
    let timer = json!({
        "type": "timer", "interest_id": "x", "prompt": "tick", "enabled": true,
        "interval_secs": 3600, "armed_at": 100, "max_occurrences": 3, "expires_at": null,
    });
    let state = |armed_at, next_due, used| TimerState {
        armed_at,
        next_due,
        used,
    };
    let armed = set_attention(&record(), "watch/t", Some(&timer), 100, None).unwrap();
    assert_eq!(armed.timers["watch/t"], state(100, 3700, 0));
    let ran = AgentRecord {
        timers: Arc::new(BTreeMap::from([(
            "watch/t".to_owned(),
            state(100, 7300, 1),
        )])),
        ..armed.clone()
    };
    let with = |key: &str, value: Value| {
        let mut timer = timer.clone();
        timer[key] = value;
        timer
    };
    let later = 100 + 10 * 3600;
    // A new prompt or interval, pausing and resuming: none of them spends
    // an occurrence or moves the deadline. A new interval applies from the
    // next run.
    for edit in [
        with("prompt", json!("tock")),
        with("interval_secs", json!(60)),
        with("enabled", json!(false)),
        with("enabled", json!(true)),
    ] {
        assert_eq!(
            set_attention(&ran, "watch/t", Some(&edit), later, None)
                .unwrap()
                .timers,
            ran.timers
        );
    }
    let timers_of = |edit: Value, schedule| {
        set_attention(&ran, "watch/t", Some(&edit), later, schedule)
            .unwrap()
            .timers["watch/t"]
    };
    // A rearm is a new deadline that keeps what was used, as in Janet,
    // even when it is armed in the past.
    assert_eq!(
        timers_of(with("armed_at", json!(later)), None),
        state(later, later + 3600, 1)
    );
    assert_eq!(
        timers_of(with("armed_at", json!(200)), None),
        state(200, 3800, 1)
    );
    // An explicit rearm sets the deadline even at the same armed_at, so a
    // new interval applies from it; a restart also gives back the whole
    // budget.
    assert_eq!(
        timers_of(with("interval_secs", json!(60)), Some(Schedule::Rearm)),
        state(100, 160, 1)
    );
    assert_eq!(
        timers_of(with("armed_at", json!(later)), Some(Schedule::Restart)),
        state(later, later + 3600, 0)
    );
    // Without saved state, what was already due counts as used.
    assert_eq!(
        set_attention(&record(), "watch/t", Some(&timer), 100 + 2 * 3600 + 5, None)
            .unwrap()
            .timers["watch/t"],
        state(100, 100 + 3 * 3600, 2)
    );
    // Timer state of a removed timer goes with it.
    assert!(
        set_attention(&ran, "watch/t", None, 141, None)
            .unwrap()
            .timers
            .is_empty()
    );
}

#[test]
fn saves_attention_only_as_on_and_reads_anything_else_as_off() {
    let storage = MemoryStorage::default();
    let (off, older) = ("b".repeat(64), "c".repeat(64));
    let records = BTreeMap::from([
        (
            pubkey(),
            AgentRecord {
                attention_enabled: Some(true),
                ..record()
            },
        ),
        (off.clone(), AgentRecord::blank(&off)),
        (
            older.clone(),
            AgentRecord {
                attention_enabled: Some(false),
                ..AgentRecord::blank(&older)
            },
        ),
    ]);
    write_records(&storage, &records).unwrap();
    assert!(
        stored(&storage)["agents"][&off]
            .get("attentionEnabled")
            .is_none()
    );
    assert!(
        stored(&storage)["agents"][&older]
            .get("attentionEnabled")
            .is_some()
    );
    let read = read_records(&storage);
    assert_eq!(read[&pubkey()].attention_enabled, Some(true));
    assert!(read[&pubkey()].attention_on());
    for other in [&off, &older] {
        assert_eq!(read[other].attention_enabled, None);
        assert!(!read[other].attention_on());
    }
}

#[test]
fn validates_replaces_and_deletes_one_object_at_a_time() {
    let error = set_attention(
        &record(),
        "interest/x",
        Some(&json!({"type": "interest", "instructions": " "})),
        1,
        None,
    )
    .unwrap_err();
    assert!(error.contains("empty"), "{error}");
    let one = set_attention(
        &record(),
        "interest/x",
        Some(&json!({"type": "interest", "instructions": "a"})),
        1,
        None,
    )
    .unwrap();
    let two = set_attention(
        &one,
        "interest/x",
        Some(&json!({"type": "interest", "instructions": "b"})),
        2,
        None,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&two.attention["interest/x"]).unwrap(),
        json!({"slug": "interest/x", "value": {"type": "interest", "instructions": "b"}, "modifiedAt": 2})
    );
    assert!(
        set_attention(&two, "interest/x", None, 3, None)
            .unwrap()
            .attention
            .is_empty()
    );
}

#[test]
fn reads_nothing_from_missing_or_malformed_storage() {
    let storage = MemoryStorage::default();
    assert!(read_records(&storage).is_empty());
    storage.write("{").unwrap();
    assert!(read_records(&storage).is_empty());
    storage.write(r#"{"version": 2, "agents": {}}"#).unwrap();
    assert!(read_records(&storage).is_empty());
}

#[test]
fn the_agents2_records_move_to_the_agents_file() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("buzz.agents2.v1.json");
    let new = dir.path().join("buzz.agents.v1.json");
    // Nothing to move.
    migrate_storage(dir.path());
    assert!(!new.exists());
    std::fs::write(&old, "{\"version\":1}").unwrap();
    migrate_storage(dir.path());
    assert_eq!(std::fs::read_to_string(&new).unwrap(), "{\"version\":1}");
    assert!(!old.exists());
    // The same data again: the old file goes.
    std::fs::write(&old, "{\"version\":1}").unwrap();
    migrate_storage(dir.path());
    assert!(!old.exists());
    // Different data: neither is touched.
    std::fs::write(&old, "{\"other\":true}").unwrap();
    migrate_storage(dir.path());
    assert_eq!(std::fs::read_to_string(&old).unwrap(), "{\"other\":true}");
    assert_eq!(std::fs::read_to_string(&new).unwrap(), "{\"version\":1}");
}

#[test]
fn teams_round_trip_and_skip_what_is_not_one() {
    let storage = MemoryStorage::default();
    let team = Team {
        id: "t1".into(),
        name: "Crew".into(),
        instructions: "Ship it".into(),
        members: vec![pubkey()],
    };
    write_teams(&storage, std::slice::from_ref(&team)).unwrap();
    assert_eq!(read_teams(&storage), vec![team]);
    storage
        .write(
            &json!({"version": 1, "teams": [
                {"id": "a", "name": "A", "instructions": "", "members": ["nope", pubkey()]},
                {"id": "b", "name": 3, "instructions": "", "members": []},
                null,
            ]})
            .to_string(),
        )
        .unwrap();
    let teams = read_teams(&storage);
    assert_eq!(teams.len(), 1);
    assert_eq!(teams[0].members, vec![pubkey()]);
    storage
        .write(&json!({"version": 2, "teams": []}).to_string())
        .unwrap();
    assert!(read_teams(&storage).is_empty());
}
