use super::*;

fn stored(
    ids: &Ids,
    text: &str,
    timestamp: u64,
    root: Option<&Event>,
    alias: Option<&str>,
) -> Event {
    let thread = root.map(|root| ThreadRef {
        root_event_id: root.id,
        parent_event_id: root.id,
    });
    let agent = ids.agent.public_key().to_hex();
    let mut builder = buzz_sdk::build_message(
        ids.channel,
        text,
        thread.as_ref(),
        &[&agent],
        false,
        &[],
        &[],
    )
    .unwrap()
    .custom_created_at(nostr::Timestamp::from(timestamp));
    if let Some(alias) = alias {
        builder = builder.tag(Tag::parse(["d", &format!("buzz-acp:{alias}")]).unwrap());
    }
    builder.sign_with_keys(&ids.signer).unwrap()
}

/// The fake reproduces the SQL LIMIT before a non-NIP33 #d post-filter.
#[tokio::test]
async fn relay_fake_limits_kind9_before_alias_tag_filtering() {
    let ids = Ids::new();
    let harness = start(&ids, Timing::default());
    let alias = Uuid::new_v4().to_string();
    let old = stored(&ids, "old aliased prompt", 10, None, Some(&alias));
    let newer = stored(&ids, "new unrelated prompt", 11, None, None);
    harness.history.lock().unwrap().0 = vec![old.clone(), newer];
    let missed = harness
        .relay
        .query(json!({"kinds":[9],"#h":[ids.channel],"#d":[format!("buzz-acp:{alias}")],"limit":1}))
        .await
        .unwrap();
    assert!(
        missed.is_empty(),
        "a fake which filters #d before LIMIT conceals B5"
    );
    let exact = harness
        .relay
        .query(json!({"kinds":[9],"ids":[old.id],"#h":[ids.channel],"limit":1}))
        .await
        .unwrap();
    assert_eq!(
        exact.iter().map(|event| event.id).collect::<Vec<_>>(),
        [old.id]
    );
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

/// Replay a persisted Paseo handle after newer channel activity and >1 page
/// of same-agent roots, using only the production actor and faithful fake.
#[tokio::test]
async fn alias_load_scans_author_and_agent_candidates_across_pages() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    let alias = Uuid::new_v4().to_string();
    let root = stored(&ids, "old aliased conversation", 10, None, Some(&alias));
    let mut events = vec![root.clone()];
    for index in 0..103 {
        events.push(stored(
            &ids,
            &format!("new session {index}"),
            11,
            None,
            None,
        ));
    }
    let outsider = Keys::generate();
    events.push(
        buzz_sdk::build_message(
            ids.channel,
            "newest unrelated channel message",
            None,
            &[],
            false,
            &[],
            &[],
        )
        .unwrap()
        .custom_created_at(nostr::Timestamp::from(12))
        .sign_with_keys(&outsider)
        .unwrap(),
    );
    harness.history.lock().unwrap().0 = events;
    let (loaded, notes) = harness
        .client
        .request(1, "session/load", json!({"sessionId":alias}))
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert_eq!(texts(&notes), ["old aliased conversation"]);
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

#[tokio::test]
async fn alias_scan_bound_keeps_explicit_thread_loading_available() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    let alias = Uuid::new_v4().to_string();
    let root = stored(&ids, "old root", 10, None, Some(&alias));
    let mut events = vec![root.clone()];
    for index in 0..1000 {
        events.push(stored(&ids, &format!("new root {index}"), 11, None, None));
    }
    harness.history.lock().unwrap().0 = events;
    let (bounded, notes) = harness
        .client
        .request(1, "session/load", json!({"sessionId":alias}))
        .await;
    assert!(bounded["error"]["message"]
        .as_str()
        .unwrap()
        .contains("1000-event scan bound"));
    assert!(notes.is_empty());
    let (loaded, notes) = harness
        .client
        .request(2, "session/load", json!({"sessionId":root.id}))
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert_eq!(texts(&notes), ["old root"]);
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

#[tokio::test]
async fn list_cursor_keeps_ascending_ids_at_equal_timestamps() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    let events: Vec<_> = (0..53)
        .map(|index| stored(&ids, &format!("root {index}"), 10, None, None))
        .collect();
    let mut expected: Vec<_> = events.iter().map(|event| event.id.to_hex()).collect();
    expected.sort();
    harness.history.lock().unwrap().0 = events;
    let mut actual = Vec::new();
    let mut params = json!({});
    for request in 1..=4 {
        let (listed, _) = harness
            .client
            .request(request, "session/list", params)
            .await;
        let page = listed["result"]["sessions"].as_array().unwrap();
        actual.extend(
            page.iter()
                .map(|session| session["sessionId"].as_str().unwrap().to_owned()),
        );
        let Some(cursor) = listed["result"]["nextCursor"].as_str() else {
            break;
        };
        params = json!({"cursor":cursor});
    }
    assert_eq!(
        actual, expected,
        "same-second cursor skipped or repeated roots"
    );
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

#[tokio::test]
async fn reply_cursor_uses_the_last_row_in_relay_order() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    let root = stored(&ids, "root", 9, None, None);
    let replies: Vec<_> = (0..137)
        .map(|index| stored(&ids, &format!("reply {index}"), 10, Some(&root), None))
        .collect();
    let mut expected = replies.clone();
    expected.sort_by_key(|event| event.id);
    let mut events = vec![root.clone()];
    events.extend(replies);
    harness.history.lock().unwrap().0 = events;
    let (loaded, notes) = harness
        .client
        .request(1, "session/load", json!({"sessionId":root.id}))
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    let mut content = vec!["root".to_owned()];
    content.extend(expected.iter().map(|event| event.content.clone()));
    assert_eq!(texts(&notes), content);
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

/// Both resource caps fail only loads already in flight. Another prompt can
/// still cancel and a later load can finish before the old query completes.
#[tokio::test]
async fn overlap_overflow_preserves_other_sessions_and_later_loads() {
    for bytes in [false, true] {
        let ids = Ids::new();
        let mut harness = start(&ids, Timing::default());
        harness.relay_tx.send(RelayInbound::Live).await.unwrap();
        let (new, _) = harness.client.request(1, "session/new", json!({})).await;
        let active = new["result"]["sessionId"].as_str().unwrap().to_owned();
        harness
            .client
            .send(prompt(2, &active, "keep running"))
            .await;
        recv(&mut harness.submitted).await;
        let root = stored(&ids, "loading root", 10, None, None);
        let next = stored(&ids, "later root", 10, None, None);
        harness.history.lock().unwrap().0 = vec![root.clone(), next.clone()];
        harness.consume_query_permits.store(true, Ordering::SeqCst);
        let blocked = harness
            .query_gate
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        harness.client.send(json!({"jsonrpc":"2.0","id":3,"method":"session/load","params":{"sessionId":root.id}})).await;
        tokio::time::timeout(WAIT, harness.query_entered.notified())
            .await
            .unwrap();
        harness.query_gate.add_permits(1);
        tokio::time::timeout(WAIT, harness.query_entered.notified())
            .await
            .unwrap();
        let count = if bytes { 80 } else { 1025 };
        for index in 0..count {
            let content = if bytes {
                format!("{index}:{}", "x".repeat(60 * 1024 - 16))
            } else {
                format!("live {index}")
            };
            harness
                .inject(stored(&ids, &content, 11, Some(&root), None))
                .await;
        }
        let (alive, _) = harness.client.request(4, "initialize", json!({})).await;
        assert!(
            alive.get("result").is_some(),
            "overflow terminated the bridge"
        );
        harness
            .client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":active}}))
            .await;
        recv(&mut harness.ephemeral).await;
        harness.client.send(json!({"jsonrpc":"2.0","id":5,"method":"session/load","params":{"sessionId":next.id}})).await;
        tokio::time::timeout(WAIT, harness.query_entered.notified())
            .await
            .unwrap();
        drop(blocked);
        let mut failed = false;
        let mut loaded = false;
        let mut restored = Vec::new();
        tokio::time::timeout(WAIT, async {
            while !failed || !loaded {
                let line = harness.client.stdout.next_line().await.unwrap().unwrap();
                let frame: Value = serde_json::from_str(&line).unwrap();
                match frame["id"].as_u64() {
                    Some(3) => {
                        assert!(
                            frame["error"]["message"]
                                .as_str()
                                .unwrap()
                                .contains("overlap exceeded"),
                            "{frame}"
                        );
                        failed = true;
                    }
                    Some(5) => {
                        assert!(frame.get("result").is_some(), "{frame}");
                        loaded = true;
                    }
                    _ => {
                        if frame["params"]["sessionId"] == json!(next.id.to_hex()) {
                            restored.extend(texts(&[frame]));
                        }
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(restored, ["later root"]);
        drop(harness.client);
        harness.bridge.await.unwrap().unwrap();
    }
}
