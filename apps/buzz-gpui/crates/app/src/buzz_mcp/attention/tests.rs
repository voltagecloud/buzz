//! buzz-app's `src/buzz-mcp/attention.test.ts`: the tools over one
//! in-memory record, as the service builds the agent's attention.

use super::testing::MemoryAttention;
use super::*;
use crate::agents2::attention::TimerState;
use crate::buzz_mcp::rpc::{AttentionTools, respond_with};
use crate::buzz_mcp::tools::tools;
use crate::buzz_mcp::{BuzzClient, Context};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const CHANNEL: &str = "0b8e2a3c-1d4f-4a5b-8c6d-7e8f9a0b1c2d";
const NOW: u64 = 10_000;
const ABSENT: &str = "object-v1:absent";

struct Agent {
    api: Arc<MemoryAttention>,
    id: AtomicU64,
}

fn agent(enabled: bool, classifier: bool) -> Agent {
    Agent {
        api: Arc::new(MemoryAttention {
            enabled: enabled.into(),
            classifier,
            ..MemoryAttention::new(NOW)
        }),
        id: AtomicU64::new(0),
    }
}

impl Agent {
    fn tools(&self) -> AttentionTools {
        AttentionTools {
            offered: true,
            api: Some(self.api.clone()),
        }
    }

    /// Calls tool `name` through the RPC, as Claude does.
    async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        let id = self.id.fetch_add(1, Ordering::SeqCst) + 1;
        let answer = respond_with(
            None,
            &Context::default(),
            &json!({ "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } }),
            Some(&self.tools()),
        )
        .await;
        let text = answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if answer["result"]["isError"] == json!(true) {
            return Err(text);
        }
        Ok(serde_json::from_str(&text).expect("JSON answers"))
    }

    async fn ok(&self, name: &str, args: Value) -> Value {
        self.call(name, args).await.unwrap()
    }

    async fn refused(&self, name: &str, args: Value) -> String {
        self.call(name, args).await.unwrap_err()
    }

    /// Counts a timer run, as the service does when it delivers one.
    fn ran(&self, slug: &str, at: u64, interval: u64) {
        let mut record = self.api.record.lock().unwrap();
        let mut timers: BTreeMap<String, TimerState> = (*record.timers).clone();
        let state = timers.get_mut(slug).expect("timer state");
        state.used += 1;
        state.next_due = at + interval;
        record.timers = Arc::new(timers);
    }

    fn at(&self, now: u64) {
        self.api.now.store(now, Ordering::SeqCst);
    }

    async fn triage(&self) {
        self.ok(
            "interest_set",
            json!({ "id": "triage", "instructions": "T.", "expected_state": ABSENT }),
        )
        .await;
    }
}

fn asks(patch: Value) -> Value {
    let mut args = json!({
        "interest": "triage", "id": "asks", "type": "event", "since": 1,
        "channels": [CHANNEL], "kinds": [9],
    });
    for (key, value) in patch.as_object().unwrap() {
        if value.is_null() {
            args.as_object_mut().unwrap().remove(key);
        } else {
            args[key] = value.clone();
        }
    }
    args
}

fn names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn offers_the_attention_tools_only_when_the_session_is_given_them() {
    let ask = json!({ "id": 1, "method": "tools/list" });
    let base: Vec<String> = tools()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    let list = |attention: Option<AttentionTools>| {
        let ask = ask.clone();
        async move { names(&respond_with(None, &Context::default(), &ask, attention.as_ref()).await) }
    };
    assert_eq!(list(None).await, base);
    assert_eq!(list(Some(AttentionTools::default())).await, base);
    let all = list(Some(AttentionTools {
        offered: true,
        api: None,
    }))
    .await;
    assert_eq!(all[..base.len()], base[..]);
    assert_eq!(all[base.len()..], NAMES.map(str::to_owned)[..]);
}

#[test]
fn has_janets_verbs_and_always_the_classifier_fields() {
    let tools = attention_tools();
    let listed: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(listed, NAMES);
    for name in ["watch_add", "watch_update"] {
        let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
        assert!(tool["inputSchema"]["properties"]["classifier"].is_object());
    }
}

#[tokio::test]
async fn refuses_an_attention_call_that_the_session_was_not_given() {
    let agent = agent(true, false);
    // A ready client, so the refusal comes from the tool set, not
    // readiness.
    let client: Arc<dyn BuzzClient> = Arc::new(crate::buzz_mcp::tools::tests::Fake::default());
    let answer = respond_with(
        Some(client),
        &Context::default(),
        &json!({ "id": 1, "method": "tools/call", "params": { "name": "interest_list" } }),
        Some(&AttentionTools {
            offered: false,
            api: Some(agent.api.clone()),
        }),
    )
    .await;
    assert_eq!(answer["result"]["isError"], true);
    assert!(
        answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("No tool interest_list")
    );
}

#[tokio::test]
async fn creates_shows_and_replaces_an_interest_with_tokens() {
    let agent = agent(true, false);
    let created = agent
        .ok(
            "interest_set",
            json!({ "id": "triage", "instructions": "Triage asks.", "expected_state": ABSENT }),
        )
        .await;
    assert_eq!(created["classifier"], "unavailable");
    assert_eq!(created["result"], "found");
    assert_eq!(created["object"]["id"], "triage");
    assert_eq!(created["object"]["instructions"], "Triage asks.");
    let stale = agent
        .refused(
            "interest_set",
            json!({ "id": "triage", "instructions": "Stale.", "expected_state": ABSENT }),
        )
        .await;
    assert!(
        stale.starts_with("conflict:") && stale.ends_with("(classifier: unavailable)"),
        "{stale}"
    );
    let shown = agent.ok("interest_show", json!({ "id": "triage" })).await;
    assert_eq!(shown["expected_state"], created["expected_state"]);
    agent
        .ok(
            "interest_set",
            json!({ "id": "triage", "instructions": "Triage every ask.",
                    "expected_state": shown["expected_state"] }),
        )
        .await;
    assert_eq!(
        agent.ok("interest_list", json!({})).await,
        json!({ "classifier": "unavailable", "interests": ["triage"] })
    );
}

#[tokio::test]
async fn keeps_an_interest_until_its_watches_are_gone() {
    let agent = agent(true, false);
    agent.triage().await;
    agent
        .ok("watch_add", asks(json!({ "expected_state": ABSENT })))
        .await;
    let interest = agent.ok("interest_show", json!({ "id": "triage" })).await;
    let token = interest["expected_state"].clone();
    let used = agent
        .refused(
            "interest_remove",
            json!({ "id": "triage", "expected_state": token }),
        )
        .await;
    assert!(used.contains("watches"), "{used}");
    let watch = agent.ok("watch_show", json!({ "id": "asks" })).await;
    agent
        .ok(
            "watch_remove",
            json!({ "id": "asks", "expected_state": watch["expected_state"] }),
        )
        .await;
    agent
        .ok(
            "interest_remove",
            json!({ "id": "triage", "expected_state": token }),
        )
        .await;
    assert_eq!(
        agent.ok("interest_list", json!({})).await["interests"],
        json!([])
    );
}

#[tokio::test]
async fn scopes_watch_writes_to_the_interest_the_agent_works_for() {
    let agent = agent(true, false);
    for id in ["triage", "other"] {
        agent
            .ok(
                "interest_set",
                json!({ "id": id, "instructions": id, "expected_state": ABSENT }),
            )
            .await;
    }
    agent
        .ok("watch_add", asks(json!({ "expected_state": ABSENT })))
        .await;
    let watch = agent.ok("watch_show", json!({ "id": "asks" })).await;
    let refused = agent
        .refused(
            "watch_disable",
            json!({ "id": "asks", "interest": "other", "expected_state": watch["expected_state"] }),
        )
        .await;
    assert!(refused.starts_with("wrong-scope:"), "{refused}");
}

#[tokio::test]
async fn changes_only_the_fields_given_and_clears_optional_ones() {
    let agent = agent(true, false);
    agent.triage().await;
    let added = agent
        .ok(
            "watch_add",
            asks(json!({ "name": "Asks", "filter": "!is_reply", "expected_state": ABSENT })),
        )
        .await;
    let updated = agent
        .ok(
            "watch_update",
            json!({ "id": "asks", "kinds": [9, 40002], "clear": ["filter"],
                    "expected_state": added["expected_state"] }),
        )
        .await;
    assert_eq!(
        updated["object"],
        json!({
            "id": "asks", "type": "event", "interest_id": "triage", "name": "Asks",
            "enabled": true, "status": "enabled", "since": 1, "channels": [CHANNEL],
            "kinds": [9, 40002],
        })
    );
    let wrong = agent
        .refused(
            "watch_update",
            json!({ "id": "asks", "prompt": "no", "expected_state": updated["expected_state"] }),
        )
        .await;
    assert!(
        wrong.starts_with("invalid: Not a field of this watch type: prompt"),
        "{wrong}"
    );
}

#[tokio::test]
async fn requires_since_for_an_event_watch() {
    let agent = agent(true, false);
    agent.triage().await;
    let missing = agent
        .refused(
            "watch_add",
            asks(json!({ "since": null, "expected_state": ABSENT })),
        )
        .await;
    assert!(missing.contains("since"), "{missing}");
    let now = agent
        .ok(
            "watch_add",
            asks(json!({ "since": "now", "expected_state": ABSENT })),
        )
        .await;
    assert_eq!(now["object"]["since"], NOW);
}

#[tokio::test]
async fn runs_a_timer_once_by_default_lists_it_spent_and_rearms_only_with_room() {
    let agent = agent(true, false);
    agent.triage().await;
    let added = agent
        .ok(
            "watch_add",
            json!({ "interest": "triage", "id": "nudge", "type": "timer",
                    "prompt": "Check the queue.", "interval_secs": 60, "expected_state": ABSENT }),
        )
        .await;
    let object = &added["object"];
    assert_eq!(
        (
            &object["max_occurrences"],
            &object["armed_at"],
            &object["status"],
            &object["used"],
            &object["next_due"]
        ),
        (
            &json!(1),
            &json!(NOW),
            &json!("armed"),
            &json!(0),
            &json!(NOW + 60)
        )
    );
    agent.at(NOW + 61);
    agent.ran("watch/nudge", NOW + 61, 60);
    assert_eq!(
        agent.ok("watch_list", json!({ "status": "spent" })).await["watches"],
        json!([{ "id": "nudge", "interest_id": "triage", "type": "timer", "status": "spent" }])
    );
    let spent = agent.ok("watch_show", json!({ "id": "nudge" })).await;
    let token = spent["expected_state"].clone();
    let refused = agent
        .refused(
            "watch_rearm",
            json!({ "id": "nudge", "expected_state": token }),
        )
        .await;
    assert!(refused.contains("spent"), "{refused}");
    let rearmed = agent
        .ok(
            "watch_rearm",
            json!({ "id": "nudge", "max_occurrences": 3, "expected_state": token }),
        )
        .await;
    let object = &rearmed["object"];
    assert_eq!(object["armed_at"], NOW + 61);
    assert_eq!(object["max_occurrences"], 3);
    assert_eq!(object["status"], "armed");
    assert_eq!(object["used"], 1);
    assert_eq!(object["next_due"], NOW + 121);
}

#[tokio::test]
async fn filters_the_watch_list_by_type_status_and_search() {
    let agent = agent(true, false);
    agent.triage().await;
    agent
        .ok(
            "watch_add",
            asks(json!({ "name": "Asks", "expected_state": ABSENT })),
        )
        .await;
    agent
        .ok(
            "watch_add",
            asks(json!({ "id": "quiet", "enabled": false, "expected_state": ABSENT })),
        )
        .await;
    let ids = |args: Value| {
        let agent = &agent;
        async move {
            agent.ok("watch_list", args).await["watches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|watch| watch["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(ids(json!({})).await, ["asks", "quiet"]);
    assert_eq!(ids(json!({ "status": "disabled" })).await, ["quiet"]);
    assert!(ids(json!({ "type": "timer" })).await.is_empty());
    assert_eq!(ids(json!({ "search": "Ask" })).await, ["asks"]);
}

#[tokio::test]
async fn refuses_every_call_once_the_owner_turns_attention_off() {
    let agent = agent(false, false);
    let off = agent.refused("interest_list", json!({})).await;
    assert!(
        off.starts_with("Attention is off") && off.ends_with("(classifier: unavailable)"),
        "{off}"
    );
}

#[tokio::test]
async fn never_replaces_a_watch_on_add() {
    let agent = agent(true, false);
    agent.triage().await;
    let added = agent
        .ok(
            "watch_add",
            asks(json!({ "name": "Asks", "filter": "!is_reply", "expected_state": ABSENT })),
        )
        .await;
    for token in [added["expected_state"].clone(), json!(ABSENT)] {
        let refused = agent
            .refused("watch_add", asks(json!({ "expected_state": token })))
            .await;
        assert!(
            refused.starts_with("invalid-operation: watch asks exists; use watch_update"),
            "{refused}"
        );
    }
    let shown = agent.ok("watch_show", json!({ "id": "asks" })).await;
    assert_eq!(shown["object"]["name"], "Asks");
    assert_eq!(shown["object"]["filter"], "!is_reply");
}

#[tokio::test]
async fn turns_an_event_watch_off_and_on_and_refuses_missing_or_stale_watches() {
    let agent = agent(true, false);
    agent.triage().await;
    let added = agent
        .ok("watch_add", asks(json!({ "expected_state": ABSENT })))
        .await;
    let off = agent
        .ok(
            "watch_disable",
            json!({ "id": "asks", "expected_state": added["expected_state"] }),
        )
        .await;
    assert_eq!(off["object"]["enabled"], false);
    assert_eq!(off["object"]["status"], "disabled");
    let stale = agent
        .refused(
            "watch_enable",
            json!({ "id": "asks", "expected_state": added["expected_state"] }),
        )
        .await;
    assert!(stale.starts_with("conflict:"), "{stale}");
    let on = agent
        .ok(
            "watch_enable",
            json!({ "id": "asks", "expected_state": off["expected_state"] }),
        )
        .await;
    assert_eq!(on["object"]["status"], "enabled");
    for name in ["watch_enable", "watch_update", "watch_rearm"] {
        let missing = agent
            .refused(name, json!({ "id": "none", "expected_state": ABSENT }))
            .await;
        assert!(missing.starts_with("not-found:"), "{name}: {missing}");
    }
    let tokenless = agent.refused("watch_update", json!({ "id": "asks" })).await;
    assert!(
        tokenless.starts_with("invalid-operation: every write needs the expected_state"),
        "{tokenless}"
    );
}

#[tokio::test]
async fn turns_a_timer_on_only_by_rearming_it_never_from_the_past() {
    let agent = agent(true, false);
    agent.triage().await;
    let added = agent
        .ok(
            "watch_add",
            json!({ "interest": "triage", "id": "nudge", "type": "timer", "prompt": "Check.",
                    "interval_secs": 3600, "max_occurrences": 5, "expected_state": ABSENT }),
        )
        .await;
    let off = agent
        .ok(
            "watch_disable",
            json!({ "id": "nudge", "expected_state": added["expected_state"] }),
        )
        .await;
    assert_eq!(off["object"]["status"], "inactive");
    assert_eq!(off["object"]["used"], 0);
    let token = off["expected_state"].clone();
    for (name, args) in [
        (
            "watch_enable",
            json!({ "id": "nudge", "expected_state": token }),
        ),
        (
            "watch_update",
            json!({ "id": "nudge", "enabled": true, "expected_state": token }),
        ),
    ] {
        let refused = agent.refused(name, args).await;
        assert!(
            refused.starts_with("invalid-operation: use watch_rearm"),
            "{refused}"
        );
    }
    let past = agent
        .refused(
            "watch_rearm",
            json!({ "id": "nudge", "at": NOW - 1, "expected_state": token }),
        )
        .await;
    assert!(past.starts_with("invalid: at must be"), "{past}");
    // Ten hours later, nothing was spent while it was off.
    agent.at(NOW + 36_000);
    let rearmed = agent
        .ok(
            "watch_rearm",
            json!({ "id": "nudge", "expected_state": token }),
        )
        .await;
    let object = &rearmed["object"];
    assert_eq!(object["enabled"], true);
    assert_eq!(object["status"], "armed");
    assert_eq!(object["used"], 0);
    assert_eq!(object["next_due"], NOW + 36_000 + 3600);
}

#[tokio::test]
async fn sets_a_new_deadline_on_every_rearm_even_in_the_second_it_was_armed() {
    let agent = agent(true, false);
    agent.triage().await;
    let added = agent
        .ok(
            "watch_add",
            json!({ "interest": "triage", "id": "nudge", "type": "timer", "prompt": "Check.",
                    "interval_secs": 3600, "max_occurrences": 5, "expected_state": ABSENT }),
        )
        .await;
    assert_eq!(added["object"]["next_due"], NOW + 3600);
    let rearmed = agent
        .ok(
            "watch_rearm",
            json!({ "id": "nudge", "interval_secs": 60, "expected_state": added["expected_state"] }),
        )
        .await;
    let object = &rearmed["object"];
    assert_eq!(object["armed_at"], NOW);
    assert_eq!(object["used"], 0);
    assert_eq!(object["next_due"], NOW + 60);
}

fn classifier() -> Value {
    json!({ "questions": { "ask": { "question": "Is this a request for help?" } } })
}

#[tokio::test]
async fn a_classifier_is_saved_without_a_key_and_every_answer_says_it_will_not_run() {
    let agent = agent(true, false);
    agent.triage().await;
    let added = agent
        .ok(
            "watch_add",
            asks(json!({ "classifier": classifier(), "expected_state": ABSENT })),
        )
        .await;
    assert_eq!(added["classifier"], "unavailable");
    assert!(
        added["classifier_note"]
            .as_str()
            .unwrap()
            .contains("once a classifier is available")
    );
    assert_eq!(added["object"]["classifier"], classifier());
    let shown = agent.ok("watch_show", json!({ "id": "asks" })).await;
    assert!(
        shown["classifier_note"]
            .as_str()
            .unwrap()
            .contains("pass unchecked")
    );
    assert_eq!(
        agent.ok("watch_list", json!({})).await["watches"],
        json!([{ "id": "asks", "interest_id": "triage", "type": "event", "status": "enabled",
                 "classified": true, "classifier": "unavailable" }])
    );
    let cleared = agent
        .ok(
            "watch_update",
            json!({ "id": "asks", "clear": ["classifier"], "expected_state": added["expected_state"] }),
        )
        .await;
    assert!(cleared.get("classifier_note").is_none());
    assert!(cleared["object"].get("classifier").is_none());
}

#[tokio::test]
async fn a_classifier_carries_no_warning_when_one_can_run() {
    let agent = agent(true, true);
    agent.triage().await;
    let added = agent
        .ok(
            "watch_add",
            asks(json!({ "classifier": classifier(), "expected_state": ABSENT })),
        )
        .await;
    assert_eq!(added["classifier"], "available");
    assert!(added.get("classifier_note").is_none());
    assert!(
        agent.ok("watch_list", json!({})).await["watches"][0]
            .get("classifier")
            .is_none()
    );
}
