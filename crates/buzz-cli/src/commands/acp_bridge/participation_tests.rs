use super::*;

fn event(
    ids: &Ids,
    author: &Keys,
    content: &str,
    time: u64,
    root: Option<&Event>,
    mention: bool,
) -> Event {
    let thread = root.map(|root| ThreadRef {
        root_event_id: root.id,
        parent_event_id: root.id,
    });
    let agent = ids.agent.public_key().to_hex();
    let mentions = if mention {
        vec![agent.as_str()]
    } else {
        Vec::new()
    };
    buzz_sdk::build_message(
        ids.channel,
        content,
        thread.as_ref(),
        &mentions,
        false,
        &[],
        &[],
    )
    .unwrap()
    .custom_created_at(nostr::Timestamp::from(time))
    .sign_with_keys(author)
    .unwrap()
}

#[tokio::test]
async fn later_mentions_and_agent_replies_make_existing_roots_discoverable_and_loadable() {
    let ids = Ids::new();
    let peer = Keys::generate();
    let mut harness = start(&ids, Timing::default());
    let mentioned = event(
        &ids,
        &peer,
        "ordinary root before an agent mention",
        10,
        None,
        false,
    );
    let mention = event(
        &ids,
        &peer,
        "later reply mentions agent",
        20,
        Some(&mentioned),
        true,
    );
    let authored = event(
        &ids,
        &peer,
        "ordinary root before an agent reply",
        11,
        None,
        false,
    );
    let answer = event(
        &ids,
        &ids.agent,
        "later agent-authored reply",
        21,
        Some(&authored),
        false,
    );
    let agent_root = event(
        &ids,
        &ids.agent,
        "agent-authored root without a self mention",
        22,
        None,
        false,
    );
    let unrelated = event(&ids, &peer, "unrelated root", 12, None, false);
    // A reference to this root in an e/mention tag is not thread participation.
    let decoy = buzz_sdk::build_message(
        ids.channel,
        "agent message in a different thread",
        Some(&ThreadRef {
            root_event_id: authored.id,
            parent_event_id: authored.id,
        }),
        &[],
        false,
        &[],
        &[],
    )
    .unwrap()
    .tag(Tag::parse(["e", &unrelated.id.to_hex(), "", "mention"]).unwrap())
    .sign_with_keys(&ids.agent)
    .unwrap();
    harness.history.lock().unwrap().0 = vec![
        mentioned.clone(),
        mention,
        authored.clone(),
        answer,
        agent_root.clone(),
        unrelated.clone(),
        decoy,
    ];
    let (listed, _) = harness.client.request(1, "session/list", json!({})).await;
    let actual = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value["sessionId"].as_str().unwrap().to_owned())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        actual,
        [
            mentioned.id.to_hex(),
            authored.id.to_hex(),
            agent_root.id.to_hex()
        ]
        .into_iter()
        .collect()
    );
    for (request, root, text) in [
        (2, &mentioned, "later reply mentions agent"),
        (3, &authored, "later agent-authored reply"),
        (4, &agent_root, "agent-authored root without a self mention"),
    ] {
        let (loaded, notes) = harness
            .client
            .request(
                request,
                "session/load",
                json!({"sessionId":root.id.to_hex()}),
            )
            .await;
        assert!(loaded.get("result").is_some(), "{loaded}");
        assert!(
            texts(&notes).iter().any(|value| value.contains(text)),
            "{notes:?}"
        );
    }
    let (refused, _) = harness
        .client
        .request(
            5,
            "session/load",
            json!({"sessionId":unrelated.id.to_hex()}),
        )
        .await;
    assert!(
        refused.get("error").is_some(),
        "an unrelated e/mention must not establish participation"
    );
    // Prefix deduplication must not route a later read to a replica that
    // has not caught up with the participation event already seen by the client.
    {
        let queries = harness.relay.query_filters.lock().unwrap();
        for query in queries.iter() {
            let filters = query
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![query.clone()]);
            for filter in filters {
                if filter["kinds"] == json!([9]) {
                    assert_eq!(filter["consistency"], "strong", "{filter}");
                }
            }
        }
    }
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

#[tokio::test]
async fn participation_pages_merge_same_second_streams_and_deduplicate_threads() {
    let ids = Ids::new();
    let peer = Keys::generate();
    let mut harness = start(&ids, Timing::default());
    let mut stored = Vec::new();
    let mut expected = Vec::new();
    for index in 0..45 {
        let root = event(
            &ids,
            &peer,
            &format!("joined thread {index}"),
            10,
            None,
            false,
        );
        let author = if index % 3 == 0 { &peer } else { &ids.agent };
        let witness = event(
            &ids,
            author,
            &format!("latest participation {index}"),
            20,
            Some(&root),
            index % 3 != 1,
        );
        expected.push((witness.id, root.id.to_hex()));
        stored.extend([root.clone(), witness]);
        stored.push(event(
            &ids,
            &ids.agent,
            &format!("older agent reply {index}"),
            19,
            Some(&root),
            false,
        ));
        if index < 5 {
            for repeat in 0..20 {
                stored.push(event(
                    &ids,
                    &peer,
                    &format!("older mention {index}/{repeat}"),
                    18,
                    Some(&root),
                    true,
                ));
            }
        }
    }
    expected.sort_by_key(|(witness, _)| *witness);
    harness.history.lock().unwrap().0 = stored;
    let mut params = json!({});
    let mut actual = Vec::new();
    let mut finished = false;
    for request in 1..=10 {
        let (page, _) = harness
            .client
            .request(request, "session/list", params)
            .await;
        assert!(page.get("result").is_some(), "{page}");
        actual.extend(
            page["result"]["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["sessionId"].as_str().unwrap().to_owned()),
        );
        let Some(cursor) = page["result"]["nextCursor"].as_str() else {
            finished = true;
            break;
        };
        params = json!({"cursor":cursor});
    }
    assert!(finished, "discovery did not exhaust its finite fixture");
    assert_eq!(
        actual,
        expected
            .into_iter()
            .map(|(_, root)| root)
            .collect::<Vec<_>>(),
        "missing, repeated, or misordered canonical threads"
    );
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

#[tokio::test]
async fn participation_read_budget_returns_a_cursor_before_the_unprocessed_thread() {
    let ids = Ids::new();
    let peer = Keys::generate();
    let mut harness = start(&ids, Timing::default());
    let mut stored = Vec::new();
    let mut expected = std::collections::HashSet::new();
    for index in 0..80 {
        let root = event(&ids, &peer, &format!("root {index}"), 10, None, false);
        expected.insert(root.id.to_hex());
        stored.push(event(
            &ids,
            &ids.agent,
            &format!("new participation {index}"),
            40,
            Some(&root),
            false,
        ));
        if index < 62 {
            stored.push(event(
                &ids,
                &ids.agent,
                &format!("old duplicate {index}"),
                20,
                Some(&root),
                false,
            ));
        }
        stored.push(root);
    }
    let fresh = event(
        &ids,
        &peer,
        "older thread must survive the read budget",
        10,
        None,
        false,
    );
    expected.insert(fresh.id.to_hex());
    stored.push(event(
        &ids,
        &ids.agent,
        "only participation for older thread",
        15,
        Some(&fresh),
        false,
    ));
    stored.push(fresh);
    harness.history.lock().unwrap().0 = stored;
    let mut params = json!({});
    let mut actual = std::collections::HashSet::new();
    let mut last = None;
    let mut bounded = false;
    let mut exhausted = false;
    for request in 1..=12 {
        let before = harness.relay.query_count.load(Ordering::SeqCst);
        let (page, _) = harness
            .client
            .request(request, "session/list", params)
            .await;
        assert!(page.get("result").is_some(), "{page}");
        let reads = harness.relay.query_count.load(Ordering::SeqCst) - before;
        assert!(reads <= 64, "unbounded discovery reads: {reads}");
        bounded |= reads == 64;
        for row in page["result"]["sessions"].as_array().unwrap() {
            assert!(
                actual.insert(row["sessionId"].as_str().unwrap().to_owned()),
                "thread repeated across pages"
            );
        }
        let Some(cursor) = page["result"]["nextCursor"].as_str() else {
            exhausted = true;
            break;
        };
        assert_ne!(last.as_deref(), Some(cursor), "cursor did not advance");
        last = Some(cursor.to_owned());
        params = json!({"cursor":cursor});
    }
    assert!(bounded, "fixture did not reach the production read budget");
    assert!(exhausted);
    assert_eq!(actual, expected, "read budget skipped an unprocessed root");
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}
