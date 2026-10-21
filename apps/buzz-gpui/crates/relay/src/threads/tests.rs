//! Ports of `threads.test.ts`, `thread-window.test.ts`,
//! `thread-target.test.ts` and `thread-follows.test.ts`.

use super::*;
use crate::event::Verifier;
use crate::reader::{ReadError, ReadErrorKind};
use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};

const UUID: &str = "00000000-0000-0000-0000-000000000000";
const ORIGIN: &str = "https://relay.example:8443";

fn tags(list: &[&[&str]]) -> Vec<Tag> {
    list.iter()
        .map(|tag| Tag::custom(tag[0], tag[1..].iter().copied()))
        .collect()
}

fn signed(keys: &Keys, kind: u16, content: &str, created_at: u64, list: &[&[&str]]) -> Verified {
    let event = EventBuilder::new(Kind::from_u16(kind), content)
        .tags(tags(list))
        .custom_created_at(Timestamp::from_secs(created_at))
        .finalize(keys)
        .unwrap();
    Verifier::default().verify(event).unwrap()
}

fn ok(events: &[Verified]) -> Result<Events, ReadError> {
    Ok(events.to_vec().into())
}

fn error(kind: ReadErrorKind, message: &str) -> ReadError {
    ReadError {
        kind,
        message: message.into(),
        status: None,
        retry_after: None,
    }
}

fn json_of(filter: &Filter) -> Value {
    Value::Object(filter.clone())
}

fn contents(snapshot: &ThreadSnapshot) -> Vec<&str> {
    snapshot
        .replies
        .iter()
        .map(|row| row.content.as_str())
        .collect()
}

fn reply_ids(snapshot: &ThreadSnapshot) -> Vec<EventId> {
    snapshot.replies.iter().map(|row| row.id).collect()
}

/// Runs a completion's evidence through the session's `accept()` as the
/// session does: every view receives it, and a target fold goes on with
/// what was kept (here all of it).
fn admit(state: &mut ThreadState, completion: Completion) -> Completion {
    state.receive(&completion.admit);
    if !completion.admission {
        return completion;
    }
    let more = state.admitted(&completion.admit);
    Completion {
        admit: completion.admit,
        denied: completion.denied,
        ..more
    }
}

/// Completes a read the way the session would: verify inside `complete`,
/// then admit what it returns.
fn step(state: &mut ThreadState, read: ThreadRead, events: &[Verified]) -> Option<ThreadRead> {
    let completion = state.complete(read.token, ok(events));
    admit(state, completion).next
}

struct Fx {
    relay: Keys,
    viewer: Keys,
    alice: Keys,
    channel: String,
    clock: Arc<MessageClock>,
}

impl Fx {
    fn new(channel: &str) -> Self {
        Self {
            relay: Keys::generate(),
            viewer: Keys::generate(),
            alice: Keys::generate(),
            channel: channel.to_string(),
            clock: Arc::default(),
        }
    }

    fn config(&self, message: EventId, exact: bool) -> ThreadConfig {
        ThreadConfig {
            channel: self.channel.clone(),
            message_id: message,
            exact,
            relay_author: self.relay.public_key(),
            signing_authority: Some(self.relay.public_key()),
            viewer: self.viewer.public_key(),
            authority_host: Some(thread_authority(ORIGIN).unwrap()),
        }
    }

    fn state(&self, message: EventId, exact: bool, seed: Option<&Verified>) -> ThreadState {
        ThreadState::new(self.config(message, exact), self.clock.clone(), seed)
    }

    fn message(&self, content: &str, created_at: u64, extra: &[&[&str]]) -> Verified {
        let h = ["h", self.channel.as_str()];
        let mut list: Vec<&[&str]> = vec![&h];
        list.extend_from_slice(extra);
        signed(&self.alice, 9, content, created_at, &list)
    }

    /// A direct reply to `root`.
    fn reply(&self, root: &Verified, content: &str, created_at: u64) -> Verified {
        let root = root.id_hex();
        self.message(content, created_at, &[&["e", &root, "", "reply"]])
    }

    /// A reply under `root` to `parent`, with both markers.
    fn nested(&self, root: &Verified, parent: &str, content: &str, created_at: u64) -> Verified {
        let root = root.id_hex();
        self.message(
            content,
            created_at,
            &[&["e", &root, "", "root"], &["e", parent, "", "reply"]],
        )
    }

    fn aux(&self, kind: u16, target: &Verified, content: &str, created_at: u64) -> Verified {
        let target = target.id_hex();
        signed(&self.alice, kind, content, created_at, &[&["e", &target]])
    }

    /// The binding, encoded independently of [`thread_binding`].
    fn binding(&self, filter: &Filter) -> String {
        let first = |key: &str| filter[key][0].as_str().unwrap().to_string();
        let mut kinds: Vec<u64> = filter["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_u64().unwrap())
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        let kinds: Vec<String> = kinds.iter().map(u64::to_string).collect();
        let cursor = match filter.get("until") {
            Some(until) => format!("[{until},\"{}\"]", filter["before_id"].as_str().unwrap()),
            None => "null".into(),
        };
        let canonical = format!(
            "[\"tw\",1,\"older\",\"relay.example:8443\",\"{}\",\"{}\",\"{}\",{},{},[{}],{},{}]",
            self.viewer.public_key().to_hex(),
            first("#h"),
            first("#e"),
            filter["limit"],
            filter.get("depth_limit").cloned().unwrap_or(json!(100)),
            kinds.join(","),
            cursor,
            filter.get("include_aux").cloned().unwrap_or(json!(false)),
        );
        let digest = Sha256::digest(canonical.as_bytes());
        format!(
            "tw:1:{}",
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    }

    fn bound_with(&self, filter: &Filter, content: Value, key: &Keys, d_h_only: bool) -> Verified {
        let binding = self.binding(filter);
        let root = filter["#e"][0].as_str().unwrap().to_string();
        let (d, h, e) = (
            ["d", binding.as_str()],
            ["h", self.channel.as_str()],
            ["e", root.as_str()],
        );
        let mut list: Vec<&[&str]> = vec![&d, &h];
        if !d_h_only {
            list.push(&e);
        }
        signed(key, 39007, &content.to_string(), 1_000, &list)
    }

    fn bound(&self, filter: &Filter, cursor: Option<(u64, String)>) -> Verified {
        let next = cursor.map_or(Value::Null, |(at, id)| json!({"created_at": at, "id": id}));
        let content = json!({
            "version": 1,
            "direction": "older",
            "has_more": !next.is_null(),
            "next_cursor": next,
        });
        self.bound_with(filter, content, &self.relay, false)
    }
}

fn cursor(event: &Verified) -> Option<(u64, String)> {
    Some((event.created_at_secs(), event.id_hex()))
}

fn is_window(filter: &Filter) -> bool {
    filter.get("thread_window") == Some(&json!(true))
}

/// Runs reads to completion, answering each filter separately like the
/// reference's fixture relay, and records every request.
struct Driver {
    requests: Vec<Vec<Filter>>,
    admitted: Vec<Verified>,
    denied: bool,
}

impl Driver {
    fn new() -> Self {
        Self {
            requests: Vec::new(),
            admitted: Vec::new(),
            denied: false,
        }
    }

    fn run(
        &mut self,
        state: &mut ThreadState,
        first: Option<ThreadRead>,
        mut answer: impl FnMut(&Filter) -> Result<Vec<Verified>, ReadError>,
    ) {
        let mut next = first;
        while let Some(read) = next {
            self.requests.push(read.filters.clone());
            let mut events = Vec::new();
            let mut failure = None;
            for filter in &read.filters {
                match answer(filter) {
                    Ok(found) => events.extend(found),
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            let result = match failure {
                Some(error) => Err(error),
                None => Ok(events.into()),
            };
            let completion = state.complete(read.token, result);
            let completion = admit(state, completion);
            self.admitted.extend(completion.admit);
            self.denied |= completion.denied;
            next = completion.next;
        }
    }

    fn windows(&self) -> Vec<&Filter> {
        self.requests
            .iter()
            .filter_map(|filters| filters.first().filter(|f| is_window(f)))
            .collect()
    }
}

// ----- bindings and bounds -----

#[test]
fn binding_matches_the_relay_vector_and_normalized_authority() {
    let filter = json!({
        "thread_window": true,
        "#h": [UUID],
        "#e": ["ab".repeat(32)],
        "kinds": [9],
        "limit": 50,
    });
    let filter = filter.as_object().unwrap();
    let host = thread_authority("https://Relay.Example.:443").unwrap();
    assert_eq!(host, "relay.example");
    let expected = "tw:1:5252322dfd797ddb1d5f1150acd4cf914b9fc09e39bcf3048d25aed514b3546d";
    assert_eq!(thread_binding(filter, &host, &"ab".repeat(32)), expected);

    // Relay defaults bind like explicit values.
    let mut explicit = filter.clone();
    explicit.insert("depth_limit".into(), json!(100));
    explicit.insert("include_aux".into(), json!(false));
    explicit.insert("kinds".into(), json!([9, 9]));
    assert_eq!(thread_binding(&explicit, &host, &"ab".repeat(32)), expected);
    for (key, value) in [
        ("limit", json!(49)),
        ("depth_limit", json!(1)),
        ("include_aux", json!(true)),
        ("kinds", json!([40002])),
        ("#e", json!(["cd".repeat(32)])),
    ] {
        let mut changed = filter.clone();
        changed.insert(key.into(), value);
        assert_ne!(
            thread_binding(&changed, &host, &"ab".repeat(32)),
            expected,
            "{key} must affect the binding"
        );
    }
    assert_ne!(
        thread_binding(filter, "other.example", &"ab".repeat(32)),
        expected
    );
    assert_ne!(thread_binding(filter, &host, &"cd".repeat(32)), expected);

    assert_eq!(
        thread_authority("wss://RELAY.example.:8443").unwrap(),
        "relay.example.:8443"
    );
    assert_eq!(thread_authority("http://[::1]:3000").unwrap(), "[::1]:3000");
    assert_eq!(
        thread_authority("https://relay.example:80").unwrap(),
        "relay.example"
    );
    assert_eq!(
        thread_authority("ws://127.0.0.1:53000").unwrap(),
        "127.0.0.1:53000"
    );
    for bad in [
        "ftp://relay.example",
        "https://user:pw@relay.example",
        "relay",
    ] {
        assert!(thread_authority(bad).is_err(), "{bad}");
    }
}

#[test]
fn binding_matches_an_independent_encoder() {
    let fx = Fx::new(UUID);
    let root = EventId::from_byte_array([7; 32]);
    let host = thread_authority(ORIGIN).unwrap();
    let viewer = fx.viewer.public_key().to_hex();
    for cursor in [
        None,
        Some(ThreadCursor {
            created_at: 1_751_500_000,
            id: "f".repeat(64),
        }),
    ] {
        for limit in [INITIAL_WINDOW_SIZE, PAGE_SIZE] {
            let filter = window_filter(UUID, &root, limit, cursor.as_ref());
            assert_eq!(thread_binding(&filter, &host, &viewer), fx.binding(&filter));
        }
    }
}

#[test]
fn builders_send_only_what_each_relay_path_accepts() {
    let root = EventId::from_byte_array([1; 32]);
    let cursor = ThreadCursor {
        created_at: 7,
        id: "a".repeat(64),
    };
    let strict = window_filter(UUID, &root, 10, Some(&cursor));
    // §9.3 #15: a thread window with `consistency` is a 400.
    assert!(
        strict
            .keys()
            .all(|key| WINDOW_FIELDS.contains(&key.as_str()))
    );
    assert!(!strict.contains_key("consistency"));
    assert_eq!(
        json_of(&strict),
        json!({"kinds": [9, 40002], "#h": [UUID], "#e": [root.to_hex()], "depth_limit": 100,
               "limit": 10, "include_aux": true, "thread_window": true,
               "until": 7, "before_id": "a".repeat(64)})
    );
    assert_eq!(
        json_of(&legacy_filter("a", &root, Some(&cursor))),
        json!({"kinds": [9, 40002, 40008], "#h": ["a"], "#e": [root.to_hex()], "depth_limit": 100,
               "limit": 50, "include_aux": true, "thread_cursor": 7,
               "thread_cursor_id": "a".repeat(64)})
    );
    assert_eq!(
        json_of(&message_filter("a", &root)),
        json!({"ids": [root.to_hex()], "#h": ["a"], "limit": 1})
    );
    assert_eq!(
        json_of(&overlays_filter(&root)),
        json!({"kinds": [5, 7, 9005, 40003, 39005], "#e": [root.to_hex()], "limit": 500})
    );
    assert_eq!(
        json_of(&tombstones_filter(&[root])),
        json!({"kinds": [5, 9005], "#e": [root.to_hex()], "limit": 500})
    );
    for filter in [&strict, &legacy_filter("a", &root, None)] {
        assert!(crate::reader::canonical(vec![filter.clone()]).is_ok());
    }
    assert!(is_canonical_channel(UUID));
    assert!(is_canonical_channel("0e9d217a-2d7e-4d5e-bbbd-eec9257acfba"));
    for bad in [
        "a",
        "general",
        "0E9D217A-2D7E-4D5E-BBBD-EEC9257ACFBA",
        "0e9d217a2d7e4d5ebbbdeec9257acfba",
    ] {
        assert!(!is_canonical_channel(bad), "{bad}");
    }
}

#[test]
fn verification_rejects_every_malformed_bound() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let row = fx.reply(&root, "reply", 10);
    let filter = window_filter(UUID, &root.id, 10, None);
    let host = thread_authority(ORIGIN).unwrap();
    let viewer = fx.viewer.public_key().to_hex();
    let relay = fx.relay.public_key().to_hex();
    let verify = |events: &[Verified]| {
        verify_thread_windows(
            std::slice::from_ref(&filter),
            events,
            Some(&host),
            &viewer,
            &relay,
        )
    };
    let good = fx.bound(&filter, None);
    assert_eq!(
        verify(&[row.clone(), good.clone()]).unwrap(),
        Some(vec![ThreadBounds {
            has_more: false,
            cursor: None
        }])
    );
    // Not a thread window at all.
    assert_eq!(
        verify_thread_windows(&[message_filter(UUID, &root.id)], &[], None, "", ""),
        Ok(None)
    );
    // Missing scope is a configuration error, not a fallback.
    assert_eq!(
        verify_thread_windows(std::slice::from_ref(&filter), &[], None, &viewer, &relay),
        Err(ThreadWindowError::Invalid)
    );
    assert_eq!(
        verify(std::slice::from_ref(&row)),
        Err(ThreadWindowError::Missing { legacy_rows: true })
    );
    assert_eq!(
        verify(&[]),
        Err(ThreadWindowError::Missing { legacy_rows: false })
    );

    let body = |patch: Value| {
        let mut content = json!({"version": 1, "direction": "older", "has_more": false,
                                 "next_cursor": null});
        for (key, value) in patch.as_object().unwrap() {
            content[key] = value.clone();
        }
        content
    };
    let mut wrong_binding = filter.clone();
    wrong_binding.insert("limit".into(), json!(49));
    let cases: Vec<(&str, Vec<Verified>)> = vec![
        (
            "wrong signer",
            vec![fx.bound_with(&filter, body(json!({})), &fx.alice, false)],
        ),
        ("duplicate bounds", vec![good.clone(), good.clone()]),
        (
            "missing tags",
            vec![fx.bound_with(&filter, body(json!({})), &fx.relay, true)],
        ),
        ("wrong binding", vec![fx.bound(&wrong_binding, None)]),
        (
            "wrong version",
            vec![fx.bound_with(&filter, body(json!({"version": 2})), &fx.relay, false)],
        ),
        (
            "wrong direction",
            vec![fx.bound_with(
                &filter,
                body(json!({"direction": "newer"})),
                &fx.relay,
                false,
            )],
        ),
        (
            "wrong flag type",
            vec![fx.bound_with(
                &filter,
                body(json!({"has_more": "false"})),
                &fx.relay,
                false,
            )],
        ),
        (
            "missing cursor",
            vec![fx.bound_with(
                &filter,
                json!({"version": 1, "direction": "older", "has_more": false}),
                &fx.relay,
                false,
            )],
        ),
        (
            "bad timestamp",
            vec![fx.bound(&filter, Some((u64::MAX, "a".repeat(64))))],
        ),
        ("bad id", vec![fx.bound(&filter, Some((1, "A".repeat(64))))]),
        (
            "contradiction",
            vec![fx.bound_with(&filter, body(json!({"has_more": true})), &fx.relay, false)],
        ),
        (
            "outside chrono",
            vec![fx.bound(&filter, Some((8_210_266_876_800, "a".repeat(64))))],
        ),
        (
            "unsafe integer",
            vec![fx.bound(&filter, Some(((1 << 53) - 1, "a".repeat(64))))],
        ),
    ];
    for (name, mut events) in cases {
        events.insert(0, row.clone());
        assert_eq!(verify(&events), Err(ThreadWindowError::Invalid), "{name}");
    }
    // A negative cursor time cannot even be encoded as u64; check the body.
    assert!(
        thread_bounds(r#"{"version":1,"direction":"older","has_more":true,"next_cursor":{"created_at":-1,"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#)
            .is_err()
    );
    // chrono's inclusive maximum is fine.
    let max = fx.bound(&filter, Some((MAX_CURSOR_TIME, "a".repeat(64))));
    assert!(verify(&[max]).is_ok());
}

// ----- strict thread windows -----

#[test]
fn pages_tied_replies_newest_first_and_repairs_retained_overlays() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let rows: Vec<Verified> = (0..303)
        .map(|n| fx.reply(&root, &format!("reply {n}"), 10 + n / 3))
        .collect();
    let mut newest = rows.clone();
    newest.sort_by(|a, b| {
        b.created_at_secs()
            .cmp(&a.created_at_secs())
            .then(a.id.cmp(&b.id))
    });
    let late = fx.reply(&root, "live", 999);
    let oldest = rows[0].clone();
    let latest = newest[0].clone();
    let overlays: RefCell<Vec<Verified>> = RefCell::default();
    let answer = |f: &Filter| {
        if f.contains_key("ids") {
            return Ok(vec![root.clone()]);
        }
        assert!(is_window(f));
        assert!(!f.contains_key("thread_cursor"));
        let until = f.get("until").and_then(Value::as_u64);
        let before = f.get("before_id").and_then(Value::as_str).unwrap_or("");
        let eligible: Vec<&Verified> = newest
            .iter()
            .filter(|e| {
                until.is_none_or(|until| {
                    e.created_at_secs() < until
                        || (e.created_at_secs() == until && e.id_hex().as_str() > before)
                })
            })
            .collect();
        let limit = f["limit"].as_u64().unwrap() as usize;
        let page: Vec<Verified> = eligible.iter().take(limit).map(|e| (*e).clone()).collect();
        let last = page.last().unwrap();
        let mut response = page.clone();
        response.extend(
            overlays
                .borrow()
                .iter()
                .filter(|o| {
                    e_targets(o)
                        .into_iter()
                        .any(|t| page.iter().any(|row| row.id == t))
                })
                .cloned(),
        );
        let next = (eligible.len() > page.len())
            .then(|| cursor(last))
            .flatten();
        response.push(fx.bound(f, next));
        Ok(response)
    };
    let mut state = fx.state(root.id, false, Some(&root));
    let mut driver = Driver::new();
    let first = state.refresh();
    driver.run(&mut state, first, answer);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Ready);
    assert_eq!(snapshot.direction, Some(Direction::Older));
    assert!(snapshot.can_load_more);
    let mut first_ten: Vec<EventId> = newest[..10].iter().map(|e| e.id).collect();
    let mut shown = reply_ids(&snapshot);
    first_ten.sort();
    shown.sort();
    assert_eq!(shown, first_ten);

    state.receive(&[late.clone(), latest.clone()]);
    while state.snapshot().can_load_more {
        let more = state.load_more();
        driver.run(&mut state, more, answer);
    }
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Ready);
    assert!(!snapshot.limited);
    let mut expected: Vec<&Verified> = rows.iter().chain([&late]).collect();
    expected.sort_by(|a, b| {
        a.created_at_secs()
            .cmp(&b.created_at_secs())
            .then(a.id.cmp(&b.id))
    });
    assert_eq!(
        reply_ids(&snapshot),
        expected.iter().map(|e| e.id).collect::<Vec<_>>()
    );
    let limits: Vec<u64> = driver
        .windows()
        .iter()
        .map(|f| f["limit"].as_u64().unwrap())
        .collect();
    assert_eq!(limits, [10, 50, 50, 50, 50, 50, 50]);
    assert!(
        driver
            .windows()
            .iter()
            .all(|f| f["include_aux"] == json!(true))
    );
    // The relay's window row allowlist rejects legacy diffs.
    assert!(
        driver
            .windows()
            .iter()
            .all(|f| !f["kinds"].as_array().unwrap().contains(&json!(40008)))
    );
    assert!(driver.requests.iter().all(|filters| filters.len() == 1));

    // A repair re-reads the whole retained range and picks up an edit.
    overlays
        .borrow_mut()
        .push(fx.aux(40003, &oldest, "edited oldest", 1000));
    let before = driver.requests.len();
    let repair = state.refresh();
    driver.run(&mut state, repair, answer);
    let repair = &driver.requests[before..];
    assert_eq!(repair.len(), 8);
    assert_eq!(
        json_of(&repair[0][0]),
        json!({"ids": [root.id_hex()], "#h": [UUID], "limit": 1})
    );
    assert!(repair[1..].iter().all(|filters| is_window(&filters[0])));
    let limits: Vec<u64> = repair[1..]
        .iter()
        .map(|f| f[0]["limit"].as_u64().unwrap())
        .collect();
    assert_eq!(limits, [10, 50, 50, 50, 50, 50, 50]);
    let content_of = |state: &ThreadState, id: EventId| {
        state
            .snapshot()
            .replies
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.content.clone())
    };
    assert_eq!(
        content_of(&state, oldest.id).as_deref(),
        Some("edited oldest")
    );
    let edit = overlays.borrow()[0].clone();
    state.receive(&[fx.aux(5, &edit, "", 1001)]);
    assert_eq!(
        content_of(&state, oldest.id).as_deref(),
        Some(oldest.content.as_str())
    );
}

#[test]
fn uses_raw_scan_progress_across_empty_pages() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let pages = Cell::new(0);
    let answer = |f: &Filter| {
        if f.contains_key("ids") {
            return Ok(vec![root.clone()]);
        }
        pages.set(pages.get() + 1);
        let next = (pages.get() == 1).then(|| (20, "a".repeat(64)));
        Ok(vec![fx.bound(f, next)])
    };
    let mut state = fx.state(root.id, false, Some(&root));
    let mut driver = Driver::new();
    let first = state.refresh();
    driver.run(&mut state, first, answer);
    assert_eq!(state.snapshot().status, ThreadStatus::Ready);
    assert!(state.snapshot().can_load_more);
    assert!(state.snapshot().replies.is_empty());
    let more = state.load_more();
    driver.run(&mut state, more, answer);
    let last = driver.requests.last().unwrap();
    assert_eq!(last[0]["until"], json!(20));
    assert_eq!(last[0]["before_id"], json!("a".repeat(64)));
    assert_eq!(state.snapshot().status, ThreadStatus::Ready);
    assert!(!state.snapshot().can_load_more);
}

#[test]
fn restarts_an_old_relay_probe_in_legacy_mode_with_clean_state() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let old = fx.reply(&root, "old", 10);
    let probe_only = fx.reply(&root, "probe only", 100);
    let answer = |f: &Filter| {
        Ok(if f.contains_key("ids") {
            vec![root.clone()]
        } else if is_window(f) {
            vec![probe_only.clone()]
        } else if !f.contains_key("thread_cursor") {
            vec![old.clone()]
        } else {
            vec![]
        })
    };
    let mut state = fx.state(root.id, false, Some(&root));
    let mut driver = Driver::new();
    let first = state.refresh();
    driver.run(&mut state, first, answer);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Ready);
    assert_eq!(snapshot.direction, Some(Direction::Forward));
    assert!(snapshot.can_load_more);
    assert_eq!(reply_ids(&snapshot), [old.id]);
    assert!(!driver.admitted.iter().any(|e| e.id == probe_only.id));
    let last = driver.requests.last().unwrap();
    assert_eq!(last.len(), 2);
    assert!(!last[1].contains_key("thread_window"));
    assert!(!last[1].contains_key("until"));
    assert!(last[1]["kinds"].as_array().unwrap().contains(&json!(40008)));

    let more = state.load_more();
    driver.run(&mut state, more, answer);
    let last = driver.requests.last().unwrap();
    assert_eq!(last[1]["thread_cursor"], json!(old.created_at_secs()));
    assert_eq!(last[1]["thread_cursor_id"], json!(old.id_hex()));
    assert!(!state.snapshot().can_load_more);
}

#[test]
fn a_later_missing_bound_neither_downgrades_nor_admits_rows() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let first = fx.reply(&root, "first", 200);
    let untrusted = fx.reply(&root, "untrusted", 10);
    let calls = Cell::new(0);
    let answer = |f: &Filter| {
        if f.contains_key("ids") {
            return Ok(vec![root.clone()]);
        }
        calls.set(calls.get() + 1);
        Ok(if calls.get() == 1 {
            vec![first.clone(), fx.bound(f, cursor(&first))]
        } else {
            vec![untrusted.clone()]
        })
    };
    let mut state = fx.state(root.id, false, Some(&root));
    let mut driver = Driver::new();
    let read = state.refresh();
    driver.run(&mut state, read, answer);
    let read = state.load_more();
    driver.run(&mut state, read, answer);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Error);
    assert_eq!(snapshot.direction, Some(Direction::Older));
    assert_eq!(reply_ids(&snapshot), [first.id]);
    assert!(driver.requests.iter().all(|filters| filters.len() == 1));
    assert!(!driver.admitted.iter().any(|e| e.id == untrusted.id));
}

type BadBound<'a> = Box<dyn Fn(&Filter) -> Vec<Verified> + 'a>;

#[test]
fn rejects_bad_bounds_before_admission() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let row = fx.reply(&root, "row", 10);
    let bad: Vec<(&str, BadBound)> = vec![
        (
            "wrong signer",
            Box::new(|f| {
                let content =
                    json!({"version":1,"direction":"older","has_more":false,"next_cursor":null});
                vec![fx.bound_with(f, content, &fx.alice, false)]
            }),
        ),
        (
            "duplicate",
            Box::new(|f| vec![fx.bound(f, None), fx.bound(f, None)]),
        ),
        (
            "wrong binding",
            Box::new(|f| {
                let mut other = f.clone();
                other.insert("limit".into(), json!(49));
                vec![fx.bound(&other, None)]
            }),
        ),
        (
            "contradiction",
            Box::new(|f| {
                let content =
                    json!({"version":1,"direction":"older","has_more":true,"next_cursor":null});
                vec![fx.bound_with(f, content, &fx.relay, false)]
            }),
        ),
        (
            "outside chrono",
            Box::new(|f| vec![fx.bound(f, Some((8_210_266_876_800, "a".repeat(64))))]),
        ),
    ];
    for (name, bound) in bad {
        let mut state = fx.state(root.id, false, Some(&root));
        let mut driver = Driver::new();
        let read = state.refresh();
        driver.run(&mut state, read, |f| {
            Ok(if f.contains_key("ids") {
                vec![root.clone()]
            } else {
                let mut events = vec![row.clone()];
                events.extend(bound(f));
                events
            })
        });
        assert_eq!(state.snapshot().status, ThreadStatus::Error, "{name}");
        assert!(state.snapshot().replies.is_empty(), "{name}");
        assert!(!driver.admitted.iter().any(|e| e.id == row.id), "{name}");
        assert_eq!(driver.requests.len(), 2, "{name}");
    }
}

#[test]
fn an_empty_strict_response_is_an_error_not_a_fallback_or_the_end() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let mut state = fx.state(root.id, false, Some(&root));
    let mut driver = Driver::new();
    let read = state.refresh();
    driver.run(&mut state, read, |f| {
        Ok(if f.contains_key("ids") {
            vec![root.clone()]
        } else {
            vec![]
        })
    });
    assert_eq!(state.snapshot().status, ThreadStatus::Error);
    assert!(!state.snapshot().can_load_more);
    assert_eq!(driver.requests.len(), 2);
}

#[test]
fn relay_errors_never_downgrade_and_can_be_retried() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    for status in [401, 403, 429, 500] {
        let mut state = fx.state(root.id, false, Some(&root));
        let mut driver = Driver::new();
        let read = state.refresh();
        let kind = if status < 404 {
            ReadErrorKind::Denied
        } else {
            ReadErrorKind::Unavailable
        };
        driver.run(&mut state, read, |f| {
            if f.contains_key("ids") {
                Ok(vec![root.clone()])
            } else {
                Err(ReadError {
                    status: Some(status),
                    ..error(kind, "Relay request failed")
                })
            }
        });
        let snapshot = state.snapshot();
        if kind == ReadErrorKind::Denied {
            assert!(driver.denied);
            assert_eq!(snapshot.status, ThreadStatus::Idle);
            assert!(snapshot.root.is_none());
            assert!(snapshot.replies.is_empty());
            assert!(
                snapshot
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("no longer available")
            );
            // A revoked view refuses to read until access returns.
            assert!(state.refresh().is_none());
        } else {
            assert_eq!(snapshot.status, ThreadStatus::Error);
            let read = state.refresh();
            driver.run(&mut state, read, |f| {
                Ok(if f.contains_key("ids") {
                    vec![root.clone()]
                } else {
                    vec![fx.bound(f, None)]
                })
            });
            assert_eq!(state.snapshot().status, ThreadStatus::Ready);
        }
    }
}

#[test]
fn continuation_checks_binding_changes_and_cursor_advance() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let first = fx.reply(&root, "first", 100);
    let older = fx.reply(&root, "older", 10);
    let corrupt = Cell::new(false);
    let answer = |f: &Filter| {
        if f.contains_key("ids") {
            return Ok(vec![root.clone()]);
        }
        if !f.contains_key("until") {
            return Ok(vec![first.clone(), fx.bound(f, cursor(&first))]);
        }
        let bound = if corrupt.get() {
            let mut other = f.clone();
            other.insert("until".into(), json!(1));
            fx.bound(&other, None)
        } else {
            fx.bound(f, cursor(&first))
        };
        Ok(vec![older.clone(), bound])
    };
    let mut state = fx.state(root.id, false, Some(&root));
    let mut driver = Driver::new();
    let read = state.refresh();
    driver.run(&mut state, read, answer);
    let read = state.load_more();
    driver.run(&mut state, read, answer);
    assert_eq!(state.snapshot().status, ThreadStatus::Error);
    assert_eq!(reply_ids(&state.snapshot()), [first.id]);
    corrupt.set(true);
    let read = state.load_more();
    assert!(read.is_some());
    driver.run(&mut state, read, answer);
    assert_eq!(state.snapshot().status, ThreadStatus::Error);
    assert_eq!(reply_ids(&state.snapshot()), [first.id]);
}

#[test]
fn refresh_repairs_retained_strict_pages_and_keeps_live_deletions() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let one = fx.reply(&root, "one", 10);
    let two = fx.reply(&root, "two", 60);
    let repaired = fx.aux(40003, &one, "repaired", 500);
    let edited = Cell::new(false);
    let answer = |f: &Filter| {
        if f.contains_key("ids") {
            return Ok(vec![root.clone()]);
        }
        if !f.contains_key("until") {
            return Ok(vec![two.clone(), fx.bound(f, cursor(&two))]);
        }
        let mut events = vec![one.clone()];
        if edited.get() {
            events.push(repaired.clone());
        }
        events.push(fx.bound(f, None));
        Ok(events)
    };
    let mut state = fx.state(root.id, false, Some(&root));
    let mut driver = Driver::new();
    let read = state.refresh();
    driver.run(&mut state, read, answer);
    let read = state.load_more();
    driver.run(&mut state, read, answer);
    state.receive(&[fx.aux(5, &two, "", 501)]);
    edited.set(true);
    let before = driver.requests.len();
    // What the session does when the channel's live route is re-established.
    let read = state.refresh();
    driver.run(&mut state, read, answer);
    let repair = &driver.requests[before..];
    assert_eq!(repair.len(), 3);
    assert!(repair[0][0].contains_key("ids"));
    assert!(is_window(&repair[1][0]) && !repair[1][0].contains_key("until"));
    assert_eq!(repair[2][0]["until"], json!(two.created_at_secs()));
    assert_eq!(repair[2][0]["before_id"], json!(two.id_hex()));
    assert_eq!(state.snapshot().status, ThreadStatus::Ready);
    assert_eq!(reply_ids(&state.snapshot()), [one.id]);
    assert_eq!(contents(&state.snapshot()), ["repaired"]);
}

#[test]
fn revalidates_a_missing_strict_root_and_keeps_tombstones_on_retry() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let row = fx.reply(&root, "row", 10);
    let missing = Cell::new(false);
    let answer = |f: &Filter| {
        Ok(if f.contains_key("ids") {
            if missing.get() {
                vec![]
            } else {
                vec![root.clone()]
            }
        } else {
            vec![row.clone(), fx.bound(f, None)]
        })
    };
    let mut state = fx.state(root.id, false, Some(&root));
    let mut driver = Driver::new();
    let read = state.refresh();
    driver.run(&mut state, read, answer);
    state.receive(&[fx.aux(5, &row, "", 500)]);
    missing.set(true);
    let before = driver.requests.len();
    let read = state.refresh();
    driver.run(&mut state, read, answer);
    assert_eq!(driver.requests.len(), before + 1);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Error);
    assert!(snapshot.root.is_none());
    assert!(snapshot.replies.is_empty());
    assert!(!snapshot.can_load_more);
    assert!(
        snapshot
            .error
            .as_deref()
            .unwrap()
            .contains("original thread message is unavailable")
    );
    missing.set(false);
    let read = state.refresh();
    driver.run(&mut state, read, answer);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Ready);
    assert_eq!(snapshot.root.as_ref().unwrap().id, root.id);
    assert!(snapshot.replies.is_empty());
    assert!(!snapshot.can_load_more);
}

#[test]
fn fences_held_reads_after_revoke_clear_or_interrupt() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let row = fx.reply(&root, "late", 10);
    for action in ["revoke", "clear", "interrupt"] {
        for phase in ["root", "window"] {
            let mut state = fx.state(root.id, false, Some(&root));
            let mut read = state.refresh().unwrap();
            if phase == "window" {
                read = step(&mut state, read, std::slice::from_ref(&root)).unwrap();
                assert_eq!(read.step, ReadStep::Window);
            }
            match action {
                "revoke" => state.revoke(),
                "clear" => state.clear(),
                _ => state.interrupt(),
            }
            let late = if phase == "root" {
                vec![root.clone()]
            } else {
                vec![row.clone(), fx.bound(&read.filters[0], None)]
            };
            let completion = state.complete(read.token, ok(&late));
            assert!(completion.next.is_none() && completion.admit.is_empty());
            let snapshot = state.snapshot();
            assert_eq!(snapshot.status, ThreadStatus::Idle, "{action} {phase}");
            assert!(snapshot.replies.is_empty(), "{action} {phase}");
            if action != "interrupt" {
                assert!(snapshot.root.is_none(), "{action} {phase}");
            } else {
                assert_eq!(snapshot.error.as_deref(), Some(INTERRUPTED));
            }
        }
    }
}

#[test]
fn accepts_and_echoes_the_maximum_cursor() {
    let fx = Fx::new(UUID);
    let root = fx.message("root", 1, &[]);
    let max = (MAX_CURSOR_TIME, "a".repeat(64));
    let answer = |f: &Filter| {
        Ok(if f.contains_key("ids") {
            vec![root.clone()]
        } else {
            vec![fx.bound(f, (!f.contains_key("until")).then(|| max.clone()))]
        })
    };
    let mut state = fx.state(root.id, false, Some(&root));
    let mut driver = Driver::new();
    let read = state.refresh();
    driver.run(&mut state, read, answer);
    assert_eq!(state.snapshot().status, ThreadStatus::Ready);
    assert!(state.snapshot().can_load_more);
    let read = state.load_more();
    driver.run(&mut state, read, answer);
    let last = driver.requests.last().unwrap();
    assert_eq!(last[0]["until"], json!(MAX_CURSOR_TIME));
    assert_eq!(last[0]["before_id"], json!("a".repeat(64)));
    assert!(!state.snapshot().can_load_more);
}

// ----- legacy traversal (non-UUID channel) -----

fn legacy() -> (Fx, Verified) {
    let fx = Fx::new("a");
    let root = fx.message("Root", 1, &[]);
    (fx, root)
}

#[test]
fn seeds_the_root_and_observed_replies_without_a_read() {
    let (fx, root) = legacy();
    let observed = fx.nested(&root, &root.id_hex(), "Observed reply", 2);
    let mut state = fx.state(root.id, false, Some(&root));
    assert_eq!(state.snapshot().root.as_ref().unwrap().id, root.id);
    assert!(state.receive(std::slice::from_ref(&observed)));
    assert_eq!(reply_ids(&state.snapshot()), [observed.id]);
}

#[test]
fn legacy_cursors_ignore_live_rows_and_overlay_timestamps() {
    let (fx, root) = legacy();
    let r = root.id_hex();
    let first = fx.nested(&root, &r, "first", 10);
    let second = fx.nested(&root, &r, "second", 20);
    let live = fx.nested(&root, &r, "live", 100);
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    assert_eq!(read.step, ReadStep::Legacy);
    assert_eq!(
        read.filters.iter().map(json_of).collect::<Vec<_>>(),
        [
            json!({"ids": [r], "#h": ["a"], "limit": 1}),
            json!({"kinds": [9, 40002, 40008], "#h": ["a"], "#e": [r], "depth_limit": 100,
                   "include_aux": true, "limit": 50}),
        ]
    );
    state.receive(std::slice::from_ref(&live));
    let edit = fx.aux(40003, &first, "edited", 200);
    assert!(
        step(
            &mut state,
            read,
            &[root.clone(), first, second.clone(), edit]
        )
        .is_none()
    );
    assert_eq!(contents(&state.snapshot()), ["edited", "second", "live"]);
    let read = state.load_more().unwrap();
    assert_eq!(read.filters[1]["thread_cursor"], json!(20));
    assert_eq!(read.filters[1]["thread_cursor_id"], json!(second.id_hex()));
    step(
        &mut state,
        read,
        &[root.clone(), fx.nested(&root, &r, "third", 30)],
    );
    let times: Vec<u64> = state
        .snapshot()
        .replies
        .iter()
        .map(|row| row.created_at)
        .collect();
    assert_eq!(times, [10, 20, 30, 100]);
}

#[test]
fn legacy_pages_ties_count_unrendered_kinds_and_stay_continuable() {
    let (fx, root) = legacy();
    let r = root.id_hex();
    let mut tied = [
        fx.nested(&root, &r, "one", 10),
        fx.nested(&root, &r, "two", 10),
    ];
    tied.sort_by_key(|e| e.id);
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    step(
        &mut state,
        read,
        &[root.clone(), tied[0].clone(), tied[1].clone()],
    );
    let read = state.load_more().unwrap();
    assert_eq!(read.filters[1]["thread_cursor"], json!(10));
    assert_eq!(read.filters[1]["thread_cursor_id"], json!(tied[1].id_hex()));
    let other = signed(
        &fx.alice,
        45003,
        "other content kind",
        11,
        &[&["h", "a"], &["e", &r, "", "reply"]],
    );
    step(&mut state, read, &[root.clone(), other.clone()]);
    assert!(state.snapshot().can_load_more);
    let read = state.load_more().unwrap();
    assert_eq!(read.filters[1]["thread_cursor"], json!(11));
    assert_eq!(read.filters[1]["thread_cursor_id"], json!(other.id_hex()));
    step(&mut state, read, std::slice::from_ref(&root));
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Ready);
    assert!(!snapshot.can_load_more && !snapshot.limited);
}

#[test]
fn resolves_a_selected_nested_reply_to_its_root() {
    let (fx, root) = legacy();
    let selected = fx.nested(&root, &"b".repeat(64), "nested", 3);
    let mut state = fx.state(selected.id, false, None);
    let read = state.refresh().unwrap();
    assert_eq!(read.step, ReadStep::Selected);
    let page = step(&mut state, read, std::slice::from_ref(&selected)).unwrap();
    assert_eq!(page.filters[1]["#e"], json!([root.id_hex()]));
    step(&mut state, page, &[root.clone(), selected]);
    assert_eq!(state.snapshot().root.as_ref().unwrap().id, root.id);
}

#[test]
fn keeps_row_identity_across_status_changes_pages_and_failures() {
    let (fx, root) = legacy();
    let r = root.id_hex();
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    let loading = state.snapshot();
    assert_eq!(loading.status, ThreadStatus::Loading);
    step(
        &mut state,
        read,
        &[root.clone(), fx.nested(&root, &r, "first", 10)],
    );
    let ready = state.snapshot();
    assert!(Arc::ptr_eq(
        ready.root.as_ref().unwrap(),
        loading.root.as_ref().unwrap()
    ));

    let read = state.load_more().unwrap();
    let loading = state.snapshot();
    assert_eq!(loading.status, ThreadStatus::Loading);
    assert!(Arc::ptr_eq(
        loading.root.as_ref().unwrap(),
        ready.root.as_ref().unwrap()
    ));
    assert!(Arc::ptr_eq(&loading.replies, &ready.replies));
    step(
        &mut state,
        read,
        &[root.clone(), fx.nested(&root, &r, "second", 20)],
    );
    let next = state.snapshot();
    assert!(Arc::ptr_eq(
        next.root.as_ref().unwrap(),
        ready.root.as_ref().unwrap()
    ));
    assert!(Arc::ptr_eq(&next.replies[0], &ready.replies[0]));
    assert_eq!(contents(&next), ["first", "second"]);

    let read = state.load_more().unwrap();
    let completion = state.complete(
        read.token,
        Err(error(ReadErrorKind::Unavailable, "offline")),
    );
    assert!(completion.next.is_none());
    let failed = state.snapshot();
    assert_eq!(failed.status, ThreadStatus::Error);
    assert_eq!(failed.error.as_deref(), Some("offline"));
    assert!(Arc::ptr_eq(
        failed.root.as_ref().unwrap(),
        next.root.as_ref().unwrap()
    ));
    assert!(Arc::ptr_eq(&failed.replies, &next.replies));
    assert!(Arc::ptr_eq(&failed.tree, &next.tree));
}

#[test]
fn ignores_duplicate_and_unrelated_live_traffic() {
    let (fx, root) = legacy();
    let first = fx.nested(&root, &root.id_hex(), "first", 10);
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    step(&mut state, read, &[root.clone(), first.clone()]);
    state.take_snapshot();
    let before = state.snapshot();
    let elsewhere = signed(&fx.alice, 9, "another channel", 11, &[&["h", "b"]]);
    assert!(!state.receive(&[first, elsewhere, fx.message("not a reply", 12, &[])]));
    assert!(Arc::ptr_eq(&before, &state.snapshot()));
    assert!(state.take_snapshot().is_none());
}

#[test]
fn an_admin_moderator_delete_hides_a_reply_once_the_admins_are_known() {
    let (fx, root) = legacy();
    let reply = fx.nested(&root, &root.id_hex(), "spam", 10);
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    step(&mut state, read, &[root.clone(), reply.clone()]);
    let admin = Keys::generate();
    let delete = |keys: &Keys| {
        let target = reply.id_hex();
        let h = ["h", fx.channel.as_str()];
        signed(
            keys,
            kinds::MODERATOR_DELETE,
            "",
            11,
            &[&h, &["e", &target]],
        )
    };
    // A stranger's 9005 never applies; an admin's waits for the 39001.
    state.receive(&[delete(&Keys::generate()), delete(&admin)]);
    assert_eq!(reply_ids(&state.snapshot()), [reply.id]);
    assert!(state.set_admins(&[admin.public_key()].into()));
    assert!(reply_ids(&state.snapshot()).is_empty());
    assert!(!state.set_admins(&[admin.public_key()].into()));
}

#[test]
fn follows_nested_replies_and_their_overlays_but_not_bare_references() {
    let (fx, root) = legacy();
    let r = root.id_hex();
    let parent = fx.nested(&root, &r, "parent", 10);
    let child = fx.nested(&root, &parent.id_hex(), "child", 11);
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    step(&mut state, read, &[root.clone(), parent.clone()]);
    let wrong_channel = signed(
        &fx.alice,
        9,
        "wrong",
        12,
        &[&["h", "b"], &["e", &r, "", "reply"]],
    );
    state.receive(&[
        child.clone(),
        fx.message("reference only", 12, &[&["e", &r]]),
        wrong_channel,
    ]);
    state.receive(&[fx.aux(40003, &child, "child edited", 13)]);
    assert_eq!(contents(&state.snapshot()), ["parent", "child edited"]);
    let tree = state.snapshot().tree.clone();
    assert_eq!(tree.top_level.len(), 1);
    assert_eq!(tree.children_of(&parent.id)[0].id, child.id);
    state.receive(&[fx.aux(5, &child, "", 14)]);
    assert_eq!(contents(&state.snapshot()), ["parent"]);
}

#[test]
fn a_missing_root_hides_content_and_the_page_cap_limits() {
    let (fx, root) = legacy();
    let r = root.id_hex();
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    step(&mut state, read, &[]);
    assert!(state.snapshot().root.is_none());
    assert_eq!(state.snapshot().status, ThreadStatus::Error);
    let read = state.refresh().unwrap();
    step(
        &mut state,
        read,
        &[root.clone(), fx.nested(&root, &r, "1", 10)],
    );
    for page in 2..=10u64 {
        let read = state.load_more().unwrap();
        step(
            &mut state,
            read,
            &[
                root.clone(),
                fx.nested(&root, &r, &page.to_string(), page * 10),
            ],
        );
    }
    let snapshot = state.snapshot();
    assert!(snapshot.limited && !snapshot.can_load_more);
    assert_eq!(snapshot.replies.len(), 10);
    assert!(state.load_more().is_none());
}

#[test]
fn oversized_evidence_clears_instead_of_evicting() {
    let (fx, root) = legacy();
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    let huge = fx.nested(&root, &root.id_hex(), &"x".repeat(4 << 20), 2);
    step(&mut state, read, &[root.clone(), huge]);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Error);
    assert!(snapshot.limited && !snapshot.can_load_more);
    assert!(snapshot.root.is_none() && snapshot.replies.is_empty());
    assert_eq!(snapshot.error.as_deref(), Some(MEMORY_LIMIT));
}

#[test]
fn keeps_live_tombstones_and_edits_when_a_repair_omits_them() {
    let (fx, root) = legacy();
    let r = root.id_hex();
    let deleted = fx.nested(&root, &r, "deleted", 10);
    let edited = fx.nested(&root, &r, "original", 11);
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    step(
        &mut state,
        read,
        &[root.clone(), deleted.clone(), edited.clone()],
    );
    state.receive(&[
        fx.aux(5, &deleted, "", 20),
        fx.aux(40003, &edited, "edited", 20),
    ]);
    assert_eq!(contents(&state.snapshot()), ["edited"]);
    let read = state.refresh().unwrap();
    step(&mut state, read, &[root.clone(), deleted, edited]);
    assert_eq!(contents(&state.snapshot()), ["edited"]);
}

#[test]
fn deleted_edits_and_reactions_stay_removed_in_any_order() {
    let (fx, root) = legacy();
    let row = fx.nested(&root, &root.id_hex(), "original", 10);
    let edit = fx.aux(40003, &row, "edited", 11);
    let reaction = signed(&fx.viewer, 7, "+", 11, &[&["e", &row.id_hex()]]);
    let remove_edit = fx.aux(5, &edit, "", 12);
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    // The deletion of the edit arrives before the edit itself.
    step(
        &mut state,
        read,
        &[
            root.clone(),
            row.clone(),
            remove_edit,
            edit,
            reaction.clone(),
        ],
    );
    let first = state.snapshot().replies[0].clone();
    assert_eq!(first.content, "original");
    assert_eq!(first.reactions.len(), 1);
    assert_eq!(first.reactions[0].content, "+");
    // Only the reaction's own signer can delete it.
    state.receive(&[fx.aux(5, &reaction, "", 13)]);
    assert_eq!(state.snapshot().replies[0].reactions[0].events.len(), 1);
    state.receive(&[signed(&fx.viewer, 5, "", 13, &[&["e", &reaction.id_hex()]])]);
    assert!(state.snapshot().replies[0].reactions.is_empty());
}

#[test]
fn tombstones_survive_missing_root_recovery_and_clear_forgets() {
    let (fx, root) = legacy();
    let row = fx.nested(&root, &root.id_hex(), "deleted", 10);
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    step(&mut state, read, &[root.clone(), row.clone()]);
    state.receive(&[fx.aux(5, &row, "", 20)]);
    let read = state.refresh().unwrap();
    step(&mut state, read, &[]);
    assert!(state.snapshot().root.is_none());
    let read = state.refresh().unwrap();
    step(&mut state, read, &[root.clone(), row.clone()]);
    assert_eq!(state.snapshot().root.as_ref().unwrap().id, root.id);
    assert!(state.snapshot().replies.is_empty());

    let held = state.refresh().unwrap();
    state.clear();
    assert!(state.snapshot().root.is_none());
    assert!(step(&mut state, held, &[root.clone(), row]).is_none());
    assert!(state.snapshot().root.is_none());
    // Nothing is related to a cleared view until it reads again.
    assert!(!state.receive(std::slice::from_ref(&root)));
}

#[test]
fn a_non_advancing_legacy_cursor_keeps_readable_rows() {
    let (fx, root) = legacy();
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    state.complete(
        read.token,
        Err(error(ReadErrorKind::Unavailable, "offline")),
    );
    assert_eq!(state.snapshot().status, ThreadStatus::Error);
    assert_eq!(state.snapshot().root.as_ref().unwrap().id, root.id);
    let one = fx.nested(&root, &root.id_hex(), "one", 10);
    let read = state.refresh().unwrap();
    step(&mut state, read, &[root.clone(), one.clone()]);
    let read = state.load_more().unwrap();
    step(&mut state, read, &[root.clone(), one]);
    assert!(
        state
            .snapshot()
            .error
            .as_deref()
            .unwrap()
            .contains("did not advance")
    );
    assert_eq!(state.snapshot().replies.len(), 1);
}

#[test]
fn attributes_relay_signed_output_only_under_the_signing_authority() {
    let (fx, _) = legacy();
    let alice = fx.alice.public_key().to_hex();
    let output = signed(
        &fx.relay,
        9,
        "Scheduled",
        1,
        &[&["p", &alice], &["h", "a"], &["buzz:workflow", "true"]],
    );
    let state = fx.state(output.id, false, Some(&output));
    assert_eq!(
        state.snapshot().root.as_ref().unwrap().author,
        fx.alice.public_key()
    );
    let mut config = fx.config(output.id, false);
    config.signing_authority = None;
    let state = ThreadState::new(config, fx.clock.clone(), Some(&output));
    assert_eq!(
        state.snapshot().root.as_ref().unwrap().author,
        fx.relay.public_key()
    );
}

#[test]
fn merges_local_operations_with_their_delivery() {
    let (fx, root) = legacy();
    let r = root.id_hex();
    let first = fx.nested(&root, &r, "first", 10);
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    step(&mut state, read, &[root.clone(), first.clone()]);
    let root_row = state.snapshot().root.clone().unwrap();
    let delivery = |status| LocalDelivery {
        status,
        error: None,
    };
    let sending = fx.nested(&root, &r, "sending", 20);
    let failed = fx.nested(&root, &r, "failed", 21);
    let failed_edit = fx.aux(40003, &first, "failed edit", 22);
    let reaction = signed(&fx.viewer, 7, "+", 22, &[&["e", &first.id_hex()]]);
    let elsewhere = signed(&fx.viewer, 9, "elsewhere", 23, &[&["h", "b"]]);
    state.set_local(&[
        (sending.clone(), delivery(Delivery::Sending)),
        (failed.clone(), delivery(Delivery::Failed)),
        (failed_edit, delivery(Delivery::Failed)),
        (reaction, delivery(Delivery::Sending)),
        (elsewhere, delivery(Delivery::Sending)),
    ]);
    let snapshot = state.snapshot();
    assert_eq!(contents(&snapshot), ["first", "sending", "failed"]);
    assert_eq!(snapshot.replies[0].reactions.len(), 1);
    assert_eq!(
        snapshot.replies[1].delivery.as_ref().unwrap().status,
        Delivery::Sending
    );
    assert_eq!(
        snapshot.replies[2].delivery.as_ref().unwrap().status,
        Delivery::Failed
    );
    assert!(Arc::ptr_eq(snapshot.root.as_ref().unwrap(), &root_row));
    // The echo lands: the row keeps showing, now confirmed by the relay.
    state.receive(std::slice::from_ref(&sending));
    state.set_local(&[(sending, delivery(Delivery::Seen))]);
    assert_eq!(contents(&state.snapshot()), ["first", "sending"]);
}

#[test]
fn a_refresh_during_a_run_runs_again_afterwards() {
    let (fx, root) = legacy();
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    assert!(state.loading());
    assert!(state.refresh().is_none());
    assert!(state.load_more().is_none());
    let again = step(&mut state, read, std::slice::from_ref(&root)).expect("a second refresh");
    assert_eq!(again.step, ReadStep::Legacy);
    assert!(step(&mut state, again, std::slice::from_ref(&root)).is_none());
    assert!(!state.loading());
}

// ----- exact targets -----

fn exact() -> (Fx, Verified, Verified) {
    let fx = Fx::new("a");
    let root = fx.message("Old root", 1, &[]);
    let reply = fx.reply(&root, "Selected reply", 10_000);
    (fx, root, reply)
}

/// Target and overlay reads; returns the first traversal read.
fn target_read(
    state: &mut ThreadState,
    target: &Verified,
    overlays: &[Verified],
) -> Option<ThreadRead> {
    let read = state.refresh().unwrap();
    assert_eq!(read.step, ReadStep::Target);
    let read = step(state, read, std::slice::from_ref(target))?;
    assert_eq!(read.step, ReadStep::Overlays);
    let read = step(state, read, overlays)?;
    if overlays.is_empty() {
        return Some(read);
    }
    assert_eq!(read.step, ReadStep::Tombstones);
    step(state, read, &[])
}

#[test]
fn exact_folds_overlays_and_their_deletions_before_showing_the_target() {
    let (fx, root, reply) = exact();
    let mut state = fx.state(reply.id, true, None);
    let read = state.refresh().unwrap();
    let overlays = step(&mut state, read, std::slice::from_ref(&reply)).unwrap();
    assert!(!overlays.filters[0].contains_key("#h"));
    let edit = fx.aux(40003, &reply, "Edited", 10_001);
    let reaction = fx.aux(7, &reply, "+", 10_001);
    let tombstones = step(&mut state, overlays, &[edit.clone(), reaction.clone()]).unwrap();
    assert_eq!(tombstones.filters[0]["kinds"], json!([5, 9005]));
    assert!(state.snapshot().target.is_none());
    let page = step(&mut state, tombstones, &[fx.aux(5, &reaction, "", 10_002)]).unwrap();
    assert!(step(&mut state, page, std::slice::from_ref(&root)).is_none());
    let target = state.snapshot().target.clone().unwrap();
    assert_eq!(target.content, "Edited");
    assert!(target.reactions.is_empty());
    state.receive(&[fx.aux(5, &edit, "", 10_003)]);
    assert_eq!(
        state.snapshot().target.as_ref().unwrap().content,
        reply.content
    );
    state.receive(&[fx.aux(5, &reply, "", 10_004)]);
    assert_eq!(
        state.snapshot().target_status,
        Some(TargetStatus::Unavailable)
    );
    assert!(state.snapshot().target.is_none());
}

#[test]
fn exact_keeps_an_accessible_reply_when_its_root_is_unavailable() {
    let (fx, _, reply) = exact();
    let mut state = fx.state(reply.id, true, None);
    let page = target_read(&mut state, &reply, &[]).unwrap();
    step(&mut state, page, &[]);
    let snapshot = state.snapshot();
    assert!(snapshot.root.is_none());
    assert_eq!(snapshot.target_status, Some(TargetStatus::Ready));
    assert_eq!(snapshot.target.as_ref().unwrap().id, reply.id);
    assert_eq!(reply_ids(&snapshot), [reply.id]);
}

#[test]
fn exact_keeps_a_fetched_root_as_the_root() {
    let (fx, root, _) = exact();
    let mut state = fx.state(root.id, true, None);
    let page = target_read(&mut state, &root, &[]).unwrap();
    step(&mut state, page, std::slice::from_ref(&root));
    let snapshot = state.snapshot();
    assert_eq!(snapshot.root.as_ref().unwrap().id, root.id);
    assert_eq!(snapshot.target.as_ref().unwrap().id, root.id);
    assert_eq!(snapshot.target_status, Some(TargetStatus::Ready));
    assert!(snapshot.replies.is_empty());
}

#[test]
fn exact_distinguishes_absent_and_failed_targets_then_retries() {
    let (fx, root, reply) = exact();
    let mut state = fx.state(reply.id, true, None);
    let read = state.refresh().unwrap();
    assert!(step(&mut state, read, &[]).is_none());
    assert_eq!(
        state.snapshot().target_status,
        Some(TargetStatus::Unavailable)
    );
    let read = state.refresh().unwrap();
    state.complete(
        read.token,
        Err(error(ReadErrorKind::Unavailable, "offline")),
    );
    assert_eq!(state.snapshot().target_status, Some(TargetStatus::Error));
    let page = target_read(&mut state, &reply, &[]).unwrap();
    step(&mut state, page, std::slice::from_ref(&root));
    assert_eq!(state.snapshot().target.as_ref().unwrap().id, reply.id);
}

#[test]
fn exact_refuses_a_capped_raw_overlay_response() {
    let (fx, _, reply) = exact();
    let mut state = fx.state(reply.id, true, None);
    let read = state.refresh().unwrap();
    let overlays = step(&mut state, read, std::slice::from_ref(&reply)).unwrap();
    let other = signed(&fx.alice, 9, "Hidden", 2, &[&["h", "private"]]);
    let flood: Vec<Verified> = (0..500)
        .map(|i| fx.aux(7, &other, &i.to_string(), 10_001))
        .collect();
    let completion = state.complete(overlays.token, ok(&flood));
    assert!(completion.next.is_none() && completion.admit.is_empty());
    let snapshot = state.snapshot();
    assert_eq!(snapshot.target_status, Some(TargetStatus::Error));
    assert!(snapshot.target.is_none());
    assert_eq!(snapshot.error.as_deref(), Some(EVIDENCE_EXCEEDED));
}

#[test]
fn exact_keeps_the_last_safe_fold_through_held_repair_overlays() {
    for outcome in ["deleted", "failed", "live withdrawal"] {
        let (fx, root, reply) = exact();
        let mut state = fx.state(reply.id, true, None);
        let page = target_read(&mut state, &reply, &[]).unwrap();
        step(&mut state, page, std::slice::from_ref(&root));
        let read = state.refresh().unwrap();
        let overlays = step(&mut state, read, std::slice::from_ref(&reply)).unwrap();
        let edit = fx.aux(40003, &reply, "Unsafe intermediate edit", 10_001);
        let held = step(&mut state, overlays, std::slice::from_ref(&edit)).unwrap();
        assert_eq!(held.step, ReadStep::Tombstones);
        assert_eq!(
            state.snapshot().target.as_ref().unwrap().content,
            reply.content
        );
        assert_eq!(state.snapshot().replies[0].content, reply.content);
        match outcome {
            "failed" => {
                state.complete(
                    held.token,
                    Err(error(ReadErrorKind::Unavailable, "offline")),
                );
            }
            _ => {
                if outcome == "live withdrawal" {
                    // Resolves against staged evidence without exposing it.
                    state.receive(&[fx.aux(5, &edit, "", 10_002)]);
                    state.receive(&[fx.aux(5, &reply, "", 10_003)]);
                    assert!(state.snapshot().target.is_none(), "{outcome}");
                }
                let next = step(&mut state, held, &[fx.aux(5, &edit, "", 10_002)]);
                if outcome != "live withdrawal" {
                    step(&mut state, next.unwrap(), std::slice::from_ref(&root));
                } else {
                    assert!(next.is_none());
                }
            }
        }
        let snapshot = state.snapshot();
        if outcome == "live withdrawal" {
            assert_eq!(snapshot.target_status, Some(TargetStatus::Unavailable));
        } else {
            assert_eq!(
                snapshot.target.as_ref().unwrap().content,
                reply.content,
                "{outcome}"
            );
            assert_eq!(snapshot.replies[0].content, reply.content, "{outcome}");
        }
    }
}

#[test]
fn exact_target_survives_traversal_limits_without_becoming_a_cursor() {
    let (fx, root, reply) = exact();
    let r = root.id_hex();
    let mut state = fx.state(reply.id, true, None);
    let edit = fx.aux(40003, &reply, "Edited target", 10_001);
    let page = target_read(&mut state, &reply, &[edit]).unwrap();
    let first = fx.message("First reply", 2, &[&["e", &r, "", "reply"]]);
    step(&mut state, page, &[root.clone(), first]);
    assert_eq!(
        state.snapshot().target.as_ref().unwrap().content,
        "Edited target"
    );
    assert_eq!(
        contents(&state.snapshot()),
        ["First reply", "Edited target"]
    );
    for page in 2..=10u64 {
        let read = state.load_more().unwrap();
        assert_eq!(read.filters[1]["thread_cursor"], json!(page));
        let next = fx.message(
            &format!("Reply {page}"),
            page + 1,
            &[&["e", &r, "", "reply"]],
        );
        step(&mut state, read, &[root.clone(), next]);
    }
    let snapshot = state.snapshot();
    assert!(snapshot.limited && !snapshot.can_load_more);
    assert_eq!(snapshot.target_status, Some(TargetStatus::Ready));
    assert_eq!(snapshot.target.as_ref().unwrap().id, reply.id);
}

#[test]
fn exact_denial_revokes_and_fences_late_results() {
    let (fx, _, reply) = exact();
    let mut state = fx.state(reply.id, true, None);
    let read = state.refresh().unwrap();
    let overlays = step(&mut state, read, std::slice::from_ref(&reply)).unwrap();
    let completion = state.complete(overlays.token, Err(error(ReadErrorKind::Denied, "denied")));
    assert!(completion.denied);
    assert!(state.snapshot().target.is_none());
    assert_eq!(state.snapshot().status, ThreadStatus::Idle);
    assert!(state.refresh().is_none());

    state.regrant();
    let read = state.refresh().unwrap();
    let held = step(&mut state, read, std::slice::from_ref(&reply)).unwrap();
    state.revoke();
    assert!(step(&mut state, held, &[]).is_none());
    assert!(state.snapshot().target.is_none());
    assert!(!state.receive(std::slice::from_ref(&reply)));
}

#[test]
fn exact_seeded_tombstones_keep_a_deleted_target_hidden() {
    let (fx, _, reply) = exact();
    let mut state = fx.state(reply.id, true, None);
    state.receive(&[reply.clone(), fx.aux(5, &reply, "", 10_001)]);
    let read = state.refresh().unwrap();
    let overlays = step(&mut state, read, std::slice::from_ref(&reply)).unwrap();
    // The relay omits the deletion we already hold; it is still asked about.
    let tombstones = step(&mut state, overlays, &[]).unwrap();
    assert_eq!(tombstones.step, ReadStep::Tombstones);
    assert!(step(&mut state, tombstones, &[]).is_none());
    let snapshot = state.snapshot();
    assert_eq!(snapshot.target_status, Some(TargetStatus::Unavailable));
    assert!(snapshot.target.is_none());
}

// ----- retargeting an open thread -----

/// A legacy thread opened on its root, with two replies loaded.
fn opened() -> (Fx, Verified, Verified, Verified, ThreadState) {
    let (fx, root) = legacy();
    let first = fx.reply(&root, "First", 2);
    let second = fx.reply(&root, "Second", 3);
    let mut state = fx.state(root.id, false, Some(&root));
    let read = state.refresh().unwrap();
    let page = [root.clone(), first.clone(), second.clone()];
    assert!(step(&mut state, read, &page).is_none());
    (fx, root, first, second, state)
}

/// Answers a retarget's reads until it ends.
fn answer(
    state: &mut ThreadState,
    mut read: ThreadRead,
    respond: impl Fn(ReadStep) -> Vec<Verified>,
) -> Retarget {
    loop {
        let completion = state.complete(read.token, ok(&respond(read.step)));
        let completion = admit(state, completion);
        if let Some(outcome) = completion.retarget {
            assert!(completion.next.is_none());
            return outcome;
        }
        read = completion.next.expect("the retarget's next read");
    }
}

#[test]
fn retarget_selects_within_the_thread_without_reloading_it() {
    let (fx, root, first, second, mut state) = opened();
    let read = state.retarget(first.id).unwrap();
    assert_eq!(read.step, ReadStep::Target);
    assert!(state.selecting() && !state.loading());
    let completion = state.complete(read.token, ok(std::slice::from_ref(&first)));
    let overlays = completion.next.unwrap();
    assert_eq!(overlays.step, ReadStep::Overlays);
    // Nothing is selected until the target's overlays have folded.
    assert!(state.snapshot().target.is_none());
    let edit = fx.aux(40003, &first, "Edited first", 4);
    let completion = state.complete(overlays.token, ok(std::slice::from_ref(&edit)));
    let tombstones = completion.next.unwrap();
    assert_eq!(tombstones.step, ReadStep::Tombstones);
    assert!(state.snapshot().target.is_none());
    let completion = state.complete(tombstones.token, ok(&[]));
    // The candidate and its overlays reach the session only as one fold.
    assert!(completion.admission);
    let completion = admit(&mut state, completion);
    assert_eq!(completion.retarget, Some(Retarget::Selected));
    assert!(completion.admit.iter().any(|event| event.id == edit.id));
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Ready);
    assert_eq!(snapshot.target_status, Some(TargetStatus::Ready));
    assert_eq!(snapshot.target.as_ref().unwrap().content, "Edited first");
    assert_eq!(snapshot.root.as_ref().unwrap().id, root.id);
    assert_eq!(reply_ids(&snapshot), [first.id, second.id]);
    assert!(!state.loading() && !state.selecting());
    assert_eq!((state.message_id(), state.exact()), (&first.id, true));

    // Again, from an exact selection.
    let read = state.retarget(second.id).unwrap();
    let outcome = answer(&mut state, read, |step| match step {
        ReadStep::Target => vec![second.clone()],
        _ => Vec::new(),
    });
    assert_eq!(outcome, Retarget::Selected);
    assert_eq!(state.snapshot().target.as_ref().unwrap().id, second.id);
    // A later repair reads the new target.
    assert_eq!(state.refresh().unwrap().step, ReadStep::Target);
}

#[test]
fn retarget_selects_the_root_from_a_reply() {
    let (fx, root, reply) = exact();
    let other = fx.reply(&root, "Other reply", 10_001);
    let mut state = fx.state(reply.id, true, None);
    let page = target_read(&mut state, &reply, &[]).unwrap();
    let thread = [root.clone(), reply.clone(), other.clone()];
    assert!(step(&mut state, page, &thread).is_none());
    let before = reply_ids(&state.snapshot());
    let read = state.retarget(root.id).unwrap();
    let outcome = answer(&mut state, read, |step| match step {
        ReadStep::Target => vec![root.clone()],
        _ => Vec::new(),
    });
    assert_eq!(outcome, Retarget::Selected);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.target_status, Some(TargetStatus::Ready));
    assert_eq!(snapshot.target.as_ref().unwrap().id, root.id);
    assert_eq!(snapshot.root.as_ref().unwrap().id, root.id);
    assert_eq!(reply_ids(&snapshot), before);
    assert_eq!((state.message_id(), state.exact()), (&root.id, true));
}

#[test]
fn retarget_leaves_the_view_as_it_was_when_the_message_cannot_be_selected() {
    let (fx, root, first, _, mut state) = opened();
    let elsewhere = fx.message("Other root", 5, &[]);
    let other = fx.reply(&elsewhere, "Other reply", 6);
    let before = reply_ids(&state.snapshot());
    let unchanged = |state: &ThreadState| {
        assert!(!state.selecting());
        assert_eq!((state.message_id(), state.exact()), (&root.id, false));
        assert_eq!(reply_ids(&state.snapshot()), before);
        assert!(state.snapshot().target.is_none());
    };
    for (id, response, expected) in [
        (other.id, vec![other.clone()], Retarget::Different),
        (elsewhere.id, vec![elsewhere.clone()], Retarget::Different),
        (first.id, Vec::new(), Retarget::Unavailable),
    ] {
        let read = state.retarget(id).unwrap();
        let completion = state.complete(read.token, ok(&response));
        assert_eq!(completion.retarget, Some(expected));
        unchanged(&state);
    }
    let read = state.retarget(first.id).unwrap();
    let offline = Err(error(ReadErrorKind::Unavailable, "offline"));
    let completion = state.complete(read.token, offline);
    assert_eq!(completion.retarget, Some(Retarget::Error));
    unchanged(&state);

    // A deleted message is unavailable; its tombstone is kept, and so is
    // the earlier selection.
    let deletion = fx.aux(5, &first, "", 7);
    let read = state.retarget(first.id).unwrap();
    let outcome = answer(&mut state, read, |step| match step {
        ReadStep::Target => vec![first.clone()],
        ReadStep::Overlays => vec![deletion.clone()],
        _ => Vec::new(),
    });
    assert_eq!(outcome, Retarget::Unavailable);
    assert_eq!((state.message_id(), state.exact()), (&root.id, false));
    assert!(!reply_ids(&state.snapshot()).contains(&first.id));

    // Without a verified root, nothing is known to share it.
    let mut unread = fx.state(root.id, false, None);
    assert!(matches!(
        unread.retarget(first.id),
        Err(Retarget::Different)
    ));
}

#[test]
fn a_newer_retarget_wins_and_runs_beside_paging() {
    let (_, root, first, second, mut state) = opened();
    let older = state.retarget(first.id).unwrap();
    let newer = state.retarget(second.id).unwrap();
    let completion = state.complete(older.token, ok(std::slice::from_ref(&first)));
    assert!(completion.retarget.is_none() && completion.next.is_none());
    // A repair starts beside the selection; neither interrupts the other.
    let repair = state.refresh().unwrap();
    let outcome = answer(&mut state, newer, |step| match step {
        ReadStep::Target => vec![second.clone()],
        _ => Vec::new(),
    });
    assert_eq!(outcome, Retarget::Selected);
    assert!(state.loading());
    assert_eq!(state.snapshot().status, ThreadStatus::Loading);
    let page = [root.clone(), first.clone(), second.clone()];
    assert!(step(&mut state, repair, &page).is_none());
    let snapshot = state.snapshot();
    assert_eq!(snapshot.status, ThreadStatus::Ready);
    assert_eq!(snapshot.target.as_ref().unwrap().id, second.id);

    // A failed selection leaves a pending repair to finish.
    let repair = state.refresh().unwrap();
    let read = state.retarget(first.id).unwrap();
    let offline = Err(error(ReadErrorKind::Unavailable, "offline"));
    let completion = state.complete(read.token, offline);
    assert_eq!(completion.retarget, Some(Retarget::Error));
    assert!(state.loading());
    let next = step(&mut state, repair, std::slice::from_ref(&second)).unwrap();
    let next = step(&mut state, next, &[]).unwrap();
    assert!(step(&mut state, next, &page).is_none());
    assert_eq!(state.snapshot().status, ThreadStatus::Ready);
    assert_eq!(state.snapshot().target.as_ref().unwrap().id, second.id);
}

#[test]
fn a_retarget_during_an_exact_repair_keeps_its_selection() {
    let (fx, root, reply) = exact();
    let other = fx.reply(&root, "Other reply", 10_001);
    let mut state = fx.state(reply.id, true, None);
    let page = target_read(&mut state, &reply, &[]).unwrap();
    let thread = [root.clone(), reply.clone(), other.clone()];
    step(&mut state, page, &thread);
    let repair = state.refresh().unwrap();
    assert_eq!(repair.step, ReadStep::Target);
    let read = state.retarget(other.id).unwrap();
    let outcome = answer(&mut state, read, |step| match step {
        ReadStep::Target => vec![other.clone()],
        _ => Vec::new(),
    });
    assert_eq!(outcome, Retarget::Selected);
    // The repair's late fold of the old target neither reselects it nor
    // hides the new one; the thread is still read.
    let overlays = step(&mut state, repair, std::slice::from_ref(&reply)).unwrap();
    assert_eq!(overlays.step, ReadStep::Overlays);
    let page = step(&mut state, overlays, &[]).unwrap();
    assert_eq!(page.step, ReadStep::Legacy);
    assert!(step(&mut state, page, &thread).is_none());
    let snapshot = state.snapshot();
    assert_eq!(snapshot.target_status, Some(TargetStatus::Ready));
    assert_eq!(snapshot.target.as_ref().unwrap().id, other.id);
}

#[test]
fn access_loss_or_a_clear_cancels_a_retarget() {
    for clear in [false, true] {
        let (_, root, first, _, mut state) = opened();
        let read = state.retarget(first.id).unwrap();
        if clear {
            state.clear();
        } else {
            state.revoke();
        }
        assert!(!state.selecting());
        let completion = state.complete(read.token, ok(std::slice::from_ref(&first)));
        assert!(completion.retarget.is_none() && completion.next.is_none());
        assert_eq!(state.message_id(), &root.id);
    }
    let (_, _, first, _, mut state) = opened();
    let read = state.retarget(first.id).unwrap();
    let denied = Err(error(ReadErrorKind::Denied, "denied"));
    let completion = state.complete(read.token, denied);
    assert!(completion.denied);
    assert_eq!(completion.retarget, Some(Retarget::Cancelled));
    assert!(matches!(state.retarget(first.id), Err(Retarget::Cancelled)));
}

#[test]
fn a_candidate_refused_by_admission_never_names_the_thread() {
    let (fx, root, reply) = exact();
    let mut state = fx.state(reply.id, true, None);
    let read = state.refresh().unwrap();
    let completion = state.complete(read.token, ok(std::slice::from_ref(&reply)));
    // The bare candidate reaches no other view.
    assert!(completion.admit.is_empty());
    let completion = state.complete(completion.next.unwrap().token, ok(&[]));
    assert!(completion.admission);
    assert!(completion.admit.iter().any(|event| event.id == reply.id));
    // The session's admission refuses it.
    let completion = state.admitted(&[]);
    assert!(completion.next.is_none());
    let snapshot = state.snapshot();
    assert_eq!(snapshot.target_status, Some(TargetStatus::Unavailable));
    assert!(snapshot.root.is_none() && snapshot.target.is_none());
    assert!(snapshot.replies.is_empty());
    assert!(state.root_id().is_none());
    state.receive(&[root.clone(), fx.reply(&root, "Later live reply", 10_002)]);
    let snapshot = state.snapshot();
    assert!(snapshot.root.is_none() && snapshot.target.is_none());
    assert!(snapshot.replies.is_empty());
}

#[test]
fn a_retarget_holds_its_candidate_out_of_the_replies_until_it_folds() {
    let (fx, root, first, _, mut state) = opened();
    let candidate = fx.reply(&root, "Bare candidate", 90);
    let read = state.retarget(candidate.id).unwrap();
    let completion = state.complete(read.token, ok(std::slice::from_ref(&candidate)));
    assert!(completion.admit.is_empty());
    let overlays = completion.next.unwrap();
    assert!(!reply_ids(&state.snapshot()).contains(&candidate.id));
    let edit = fx.aux(40003, &candidate, "Complete candidate", 91);
    let completion = state.complete(overlays.token, ok(std::slice::from_ref(&edit)));
    let tombstones = completion.next.unwrap();
    assert!(completion.admit.is_empty());
    assert!(!reply_ids(&state.snapshot()).contains(&candidate.id));
    let completion = state.complete(tombstones.token, ok(&[]));
    let completion = admit(&mut state, completion);
    assert_eq!(completion.retarget, Some(Retarget::Selected));
    let snapshot = state.snapshot();
    let target = snapshot.target.as_ref().unwrap();
    assert_eq!(
        (target.id, target.content.as_str()),
        (candidate.id, "Complete candidate")
    );
    assert!(reply_ids(&snapshot).contains(&first.id));
}

#[test]
fn a_retarget_admits_finite_and_live_tombstones_of_its_staged_overlays() {
    for live in [false, true] {
        let (fx, root, _, _, mut state) = opened();
        let candidate = fx.reply(&root, "Original candidate", 90);
        let edit = fx.aux(40003, &candidate, "Withdrawn edit", 91);
        let reaction = fx.aux(7, &candidate, "+", 91);
        let withdrawn = [fx.aux(5, &edit, "", 92), fx.aux(5, &reaction, "", 92)];
        let read = state.retarget(candidate.id).unwrap();
        let completion = state.complete(read.token, ok(std::slice::from_ref(&candidate)));
        let overlays = completion.next.unwrap();
        let completion = state.complete(overlays.token, ok(&[edit.clone(), reaction.clone()]));
        let tombstones = completion.next.unwrap();
        assert_eq!(tombstones.step, ReadStep::Tombstones);
        if live {
            state.receive(&withdrawn);
        }
        let response = if live { Vec::new() } else { withdrawn.to_vec() };
        let completion = state.complete(tombstones.token, ok(&response));
        // The deletions go with the fold, so views that never saw them
        // withdraw what they delete too.
        for deletion in &withdrawn {
            assert!(completion.admit.iter().any(|event| event.id == deletion.id));
        }
        let completion = admit(&mut state, completion);
        assert_eq!(completion.retarget, Some(Retarget::Selected));
        let snapshot = state.snapshot();
        let target = snapshot.target.as_ref().unwrap();
        assert_eq!(target.content, candidate.content);
        assert!(target.reactions.is_empty());
    }
}
