//! The history cut-off of buzz-app's `runtime.test.ts`, with signed
//! events, since only verified events are folded.

use super::*;
use nostr::prelude::{EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
use std::sync::{Arc, Mutex};

const CHANNEL: &str = "channel";

/// The filters of each read.
type Filters = Arc<Mutex<Vec<Vec<Value>>>>;
type Reader =
    Box<dyn Fn(Vec<Value>) -> BoxFuture<'static, Result<Vec<BuzzEvent>, String>> + Send + Sync>;

fn signed(keys: &Keys, kind: u16, content: &str, tags: &[&[&str]]) -> BuzzEvent {
    let event = EventBuilder::new(Kind::from(kind), content)
        .tags(
            tags.iter()
                .map(|tag| Tag::parse(tag.iter().copied()).unwrap()),
        )
        .custom_created_at(Timestamp::from(10))
        .finalize(keys)
        .unwrap();
    serde_json::from_value(serde_json::to_value(event).unwrap()).unwrap()
}

/// Answers each read with the next batch, and keeps the filters.
fn reader(batches: Vec<Vec<BuzzEvent>>) -> (Filters, Reader) {
    let filters = Arc::new(Mutex::new(Vec::new()));
    let batches = Arc::new(Mutex::new(batches.into_iter()));
    let read: Reader = {
        let filters = filters.clone();
        Box::new(move |values: Vec<Value>| {
            filters.lock().unwrap().push(values);
            let batch = batches.lock().unwrap().next().unwrap_or_default();
            Box::pin(async move { Ok(batch) }) as BoxFuture<'static, _>
        })
    };
    (filters, read)
}

fn messages(section: &str, text: &str) -> Vec<String> {
    let body = text
        .strip_prefix(&format!("<{section}>\n"))
        .and_then(|text| text.strip_suffix(&format!("\n</{section}>\n")))
        .unwrap();
    let parsed: Value = serde_json::from_str(body).unwrap();
    parsed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn cuts_history_off_before_the_request_including_same_second_messages_and_edits() {
    let keys = Keys::generate();
    let root = "c".repeat(64);
    let reply = |content: &str, ms: &str| {
        signed(
            &keys,
            9,
            content,
            &[
                &["h", CHANNEL],
                &["e", root.as_str(), "", "reply"],
                &["ms", ms],
            ],
        )
    };
    let request = reply("CURRENT_REQUEST", "300");
    let previous = reply("EARLIER_MESSAGE", "100");
    let ties = [reply("TIE_A", "300"), reply("TIE_B", "300")];
    let later = reply("LATER_REQUEST", "400");
    let edit = |content: &str, ms: &str| {
        signed(
            &keys,
            40003,
            content,
            &[&["h", CHANNEL], &["e", previous.id.as_str()], &["ms", ms]],
        )
    };
    let mut found = vec![previous.clone(), request.clone(), later];
    found.extend(ties.iter().cloned());
    let (filters, read) = reader(vec![
        found,
        vec![edit("EARLIER_EDIT", "250"), edit("FUTURE_EDIT", "350")],
    ]);
    let conversation = Conversation {
        channel_id: CHANNEL.into(),
        name: "test".into(),
        root: Some(root.clone()),
        top_level: false,
    };
    let text = conversation_history(&request, &conversation, &read)
        .await
        .unwrap();
    // Same-second ties order by id, as the channel shows them.
    let mut expected = vec!["EARLIER_EDIT".to_string()];
    let mut before: Vec<&BuzzEvent> = ties.iter().filter(|tie| tie.id < request.id).collect();
    before.sort_by(|a, b| a.id.cmp(&b.id));
    expected.extend(before.iter().map(|tie| tie.content.clone()));
    assert_eq!(messages("thread-context", &text), expected);
    let filters = filters.lock().unwrap().clone();
    assert_eq!(
        filters[0],
        vec![
            json!({ "kinds": [9, 40002], "#h": [CHANNEL], "until": 10, "ids": [root], "limit": 1 }),
            json!({ "kinds": [9, 40002], "#h": [CHANNEL], "until": 10, "#e": [root], "limit": 50 }),
        ]
    );
    assert_eq!(filters[1][0]["kinds"], json!([40003, 5, 9005]));
    assert_eq!(filters[1][0]["limit"], 500);
    assert_eq!(filters[1][0]["#e"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn channel_sessions_read_the_channel_and_drop_deleted_and_forged_messages() {
    let (keys, other) = (Keys::generate(), Keys::generate());
    let message = |keys: &Keys, content: &str, ms: &str| {
        signed(keys, 9, content, &[&["h", CHANNEL], &["ms", ms]])
    };
    let request = message(&keys, "REQUEST", "500");
    let kept = message(&other, "<b>KEPT</b>", "100");
    let deleted = message(&keys, "DELETED", "200");
    let mut forged = message(&keys, "FORGED", "300");
    forged.content = "TAMPERED".into();
    let deletion = signed(
        &keys,
        5,
        "",
        &[&["h", CHANNEL], &["e", deleted.id.as_str()], &["ms", "250"]],
    );
    let (filters, read) = reader(vec![
        vec![kept.clone(), deleted, forged, request.clone()],
        vec![deletion],
    ]);
    let conversation = Conversation {
        channel_id: CHANNEL.into(),
        name: "test".into(),
        root: None,
        top_level: false,
    };
    let text = conversation_history(&request, &conversation, &read)
        .await
        .unwrap();
    assert_eq!(messages("conversation-context", &text), ["<b>KEPT</b>"]);
    // Quoted data cannot close the section.
    assert!(text.contains("\\u003cb\\u003eKEPT"));
    assert!(text.contains(&format!("\"pubkey\":\"{}\"", other.public_key().to_hex())));
    assert_eq!(
        filters.lock().unwrap()[0],
        vec![json!({ "kinds": [9, 40002], "#h": [CHANNEL], "until": 10, "limit": 50 })]
    );
}

#[tokio::test]
async fn nothing_earlier_reads_once() {
    let keys = Keys::generate();
    let request = signed(&keys, 9, "hi", &[&["h", CHANNEL]]);
    let (filters, read) = reader(vec![vec![]]);
    let conversation = Conversation {
        channel_id: CHANNEL.into(),
        name: "test".into(),
        root: None,
        top_level: false,
    };
    let text = conversation_history(&request, &conversation, &read)
        .await
        .unwrap();
    assert_eq!(
        messages("conversation-context", &text),
        Vec::<String>::new()
    );
    assert_eq!(filters.lock().unwrap().len(), 1);
}
