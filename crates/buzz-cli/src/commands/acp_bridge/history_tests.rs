use super::*;

/// A restarted ACP process discovers and replays relay threads, preserves
/// attribution, rejects out-of-scope roots and query errors, then continues live.
#[tokio::test]
async fn history_is_rebuilt_from_the_relay_and_continues_in_the_same_thread() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    harness.relay_tx.send(RelayInbound::Live).await.unwrap();
    let root = |author: &Keys, channel: Uuid, mentions: Vec<&str>, text: &str| {
        buzz_sdk::build_message(channel, text, None, &mentions, false, &[], &[])
            .unwrap()
            .sign_with_keys(author)
            .unwrap()
    };
    let agent = ids.agent.public_key().to_hex();
    let mut roots = Vec::new();
    for i in 0..23 {
        roots.push(root(
            &ids.signer,
            ids.channel,
            vec![&agent],
            &format!("stored prompt {i}"),
        ));
    }
    let chosen = roots[0].clone();
    let outsider = Keys::generate();
    let wrong_author = root(
        &outsider,
        ids.channel,
        vec![&agent],
        "teammate shared-agent thread",
    );
    let wrong_channel = root(&ids.signer, Uuid::new_v4(), vec![&agent], "another channel");
    let wrong_agent = root(&ids.signer, ids.channel, vec![], "not the configured agent");
    let mut forged = chosen.clone();
    forged.content = "tampered signed history".into();
    let wrong_reply_channel = thread_message(
        &ids.agent,
        Uuid::new_v4(),
        &chosen.id.to_hex(),
        "not this community channel",
    );
    let answer = thread_message(
        &ids.agent,
        ids.channel,
        &chosen.id.to_hex(),
        "stored answer",
    );
    let peer = thread_message(&outsider, ids.channel, &chosen.id.to_hex(), "teammate note");
    let mut events = roots.clone();
    events.extend([
        wrong_author.clone(),
        wrong_channel.clone(),
        wrong_agent.clone(),
        answer.clone(),
        peer,
        forged,
        wrong_reply_channel,
    ]);
    harness.history.lock().unwrap().0 = events;
    let (first, _) = harness.client.request(1, "session/list", json!({})).await;
    assert_eq!(first["result"]["sessions"].as_array().unwrap().len(), 20);
    let (second, _) = harness
        .client
        .request(
            2,
            "session/list",
            json!({"cursor": first["result"]["nextCursor"]}),
        )
        .await;
    assert_eq!(second["result"]["sessions"].as_array().unwrap().len(), 4);
    let (shared, shared_notes) = harness
        .client
        .request(
            30,
            "session/load",
            json!({"sessionId":wrong_author.id.to_hex()}),
        )
        .await;
    assert!(shared.get("result").is_some(), "{shared}");
    assert!(texts(&shared_notes)
        .iter()
        .any(|text| text.contains("teammate shared-agent thread")));
    for bad in [wrong_channel, wrong_agent] {
        let (refused, notes) = harness
            .client
            .request(3, "session/load", json!({"sessionId": bad.id.to_hex()}))
            .await;
        assert!(refused.get("error").is_some(), "{refused}");
        assert!(notes.is_empty());
    }
    harness.history.lock().unwrap().1 = true;
    let (failed, _) = harness.client.request(4, "session/list", json!({})).await;
    assert!(
        failed.get("error").is_some(),
        "a failed query cannot be an empty success"
    );
    harness.history.lock().unwrap().1 = false;
    let session = chosen.id.to_hex();
    let (loaded, notes) = harness
        .client
        .request(5, "session/load", json!({"sessionId":session}))
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert_eq!(notes.len(), 3);
    assert!(notes
        .iter()
        .any(|n| n["params"]["update"]["sessionUpdate"] == "user_message_chunk"));
    assert!(texts(&notes)
        .iter()
        .any(|text| text.contains("teammate note") && text.starts_with("\n\n[")));
    // A queued history echo must not appear twice. A fresh event must appear.
    harness.inject(answer).await;
    harness
        .inject(thread_message(
            &ids.agent,
            ids.channel,
            &session,
            "new live answer",
        ))
        .await;
    let live = tokio::time::timeout(WAIT, harness.client.stdout.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let live: Value = serde_json::from_str(&live).unwrap();
    assert_eq!(
        live["params"]["update"]["content"]["text"],
        "new live answer"
    );
    harness
        .client
        .send(prompt(8, &session, "continue here"))
        .await;
    let posted = recv(&mut harness.submitted).await;
    assert!(tag(&posted, "e").iter().any(|t| t.get(1) == Some(&session)));
    assert!(harness
        .history
        .lock()
        .unwrap()
        .0
        .iter()
        .any(|e| e.id == chosen.id));
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

/// Paseo persists session/new's UUID before Buzz knows the signed thread id.
/// The signed root alias makes that handle resumable without daemon-local state.
#[tokio::test]
async fn initial_paseo_handle_survives_a_bridge_restart_and_gateway_sees_owner_threads() {
    let ids = Ids::new();
    let owner = Keys::generate();
    let scope = Scope {
        owner: owner.public_key(),
        ..ids.scope()
    };
    let mut first = start_scope(scope.clone(), Timing::default());
    first.relay_tx.send(RelayInbound::Live).await.unwrap();
    let (new, _) = first.client.request(1, "session/new", json!({})).await;
    let handle = new["result"]["sessionId"].as_str().unwrap().to_owned();
    first
        .client
        .send(prompt(2, &handle, "persist my handle"))
        .await;
    let root = recv(&mut first.submitted).await;
    assert!(tag(&root, "d")
        .iter()
        .any(|parts| parts.get(1) == Some(&format!("buzz-acp:{handle}"))));
    drop(first.client);
    first.bridge.await.unwrap().unwrap();
    let mut restarted = start_scope(scope, Timing::default());
    restarted.relay_tx.send(RelayInbound::Live).await.unwrap();
    let owner_root = buzz_sdk::build_message(
        ids.channel,
        "owner's Buzz app thread",
        None,
        &[&ids.agent.public_key().to_hex()],
        false,
        &[],
        &[],
    )
    .unwrap()
    .sign_with_keys(&owner)
    .unwrap();
    restarted.history.lock().unwrap().0 = vec![root.clone(), owner_root.clone()];
    let (loaded, notes) = restarted
        .client
        .request(3, "session/load", json!({"sessionId":handle}))
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert_eq!(texts(&notes), ["persist my handle"]);
    let (listed, _) = restarted.client.request(4, "session/list", json!({})).await;
    assert_eq!(listed["result"]["sessions"].as_array().unwrap().len(), 2);
    let (loaded, notes) = restarted
        .client
        .request(
            5,
            "session/load",
            json!({"sessionId":owner_root.id.to_hex()}),
        )
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert!(notes
        .iter()
        .any(|note| note["params"]["update"]["sessionUpdate"] == "user_message_chunk"));
    restarted
        .client
        .send(prompt(6, &handle, "continue original thread"))
        .await;
    let next = recv(&mut restarted.submitted).await;
    assert!(tag(&next, "e")
        .iter()
        .any(|parts| parts.get(1) == Some(&root.id.to_hex())));
    drop(restarted.client);
    restarted.bridge.await.unwrap().unwrap();
}

/// Slow history cannot block cancellation or amplify unbounded concurrent work.
#[tokio::test]
async fn blocked_history_keeps_cancellation_responsive_and_bounds_jobs() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    harness.relay_tx.send(RelayInbound::Live).await.unwrap();
    let (new, _) = harness.client.request(1, "session/new", json!({})).await;
    let session = new["result"]["sessionId"].as_str().unwrap();
    harness
        .client
        .send(prompt(2, session, "run until cancelled"))
        .await;
    recv(&mut harness.submitted).await;
    harness.consume_query_permits.store(true, Ordering::SeqCst);
    let blocked = harness
        .query_gate
        .clone()
        .acquire_many_owned(4)
        .await
        .unwrap();
    for id in 50..54 {
        harness
            .client
            .send(json!({"jsonrpc":"2.0","id":id,"method":"session/list","params":{}}))
            .await;
        tokio::time::timeout(WAIT, harness.query_entered.notified())
            .await
            .unwrap();
    }
    let (busy, _) = harness.client.request(54, "session/list", json!({})).await;
    assert!(busy.get("error").is_some());
    harness
        .client
        .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))
        .await;
    let control = recv(&mut harness.ephemeral).await;
    assert_eq!(
        control.kind.as_u16(),
        buzz_core::kind::KIND_AGENT_OBSERVER_FRAME as u16
    );
    drop(blocked);
    let mut answered = std::collections::HashSet::new();
    while answered.len() < 4 {
        let line = tokio::time::timeout(WAIT, harness.client.stdout.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        if let Some(id @ 50..=53) = frame["id"].as_u64() {
            assert!(frame.get("result").is_some());
            answered.insert(id);
        }
    }
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

/// A reply arriving after the finite HTTP snapshot stays visible on load completion.
#[tokio::test]
async fn async_history_load_keeps_the_live_overlap() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    harness.relay_tx.send(RelayInbound::Live).await.unwrap();
    let root = buzz_sdk::build_message(
        ids.channel,
        "stored prompt",
        None,
        &[&ids.agent.public_key().to_hex()],
        false,
        &[],
        &[],
    )
    .unwrap()
    .sign_with_keys(&ids.signer)
    .unwrap();
    harness.history.lock().unwrap().0.push(root.clone());
    harness.consume_query_permits.store(true, Ordering::SeqCst);
    let blocked = harness
        .query_gate
        .clone()
        .acquire_many_owned(4)
        .await
        .unwrap();
    harness.client.send(json!({"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":root.id.to_hex()}})).await;
    tokio::time::timeout(WAIT, harness.query_entered.notified())
        .await
        .unwrap();
    // Let the root lookup complete, then hold the already-snapshotted reply query.
    harness.query_gate.add_permits(1);
    tokio::time::timeout(WAIT, harness.query_entered.notified())
        .await
        .unwrap();
    let answer = thread_message(
        &ids.agent,
        ids.channel,
        &root.id.to_hex(),
        "arrived during history request",
    );
    harness.history.lock().unwrap().0.push(answer.clone());
    harness.inject(answer).await;
    // A round trip proves the bridge consumed the live event before completing the history call.
    harness
        .client
        .send(json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}))
        .await;
    harness.client.response(2).await;
    drop(blocked);
    let (loaded, notes) = harness.client.response(1).await;
    assert!(loaded.get("result").is_some());
    assert!(texts(&notes)
        .iter()
        .any(|text| text == "arrived during history request"));
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

/// A late load cannot install an obsolete snapshot after a newer prompt starts.
#[tokio::test]
async fn a_history_result_is_fenced_when_the_session_changes() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    harness.relay_tx.send(RelayInbound::Live).await.unwrap();
    let root = buzz_sdk::build_message(
        ids.channel,
        "stored prompt",
        None,
        &[&ids.agent.public_key().to_hex()],
        false,
        &[],
        &[],
    )
    .unwrap()
    .sign_with_keys(&ids.signer)
    .unwrap();
    let session = root.id.to_hex();
    harness.history.lock().unwrap().0.push(root);
    harness
        .client
        .request(1, "session/load", json!({"sessionId":session}))
        .await;
    // Consume notifications from the first root and reply lookups.
    let _ = harness.query_entered.notified().await;
    harness.consume_query_permits.store(true, Ordering::SeqCst);
    let blocked = harness
        .query_gate
        .clone()
        .acquire_many_owned(4)
        .await
        .unwrap();
    harness
        .client
        .send(
            json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":session}}),
        )
        .await;
    tokio::time::timeout(WAIT, harness.query_entered.notified())
        .await
        .unwrap();
    harness
        .client
        .send(prompt(3, &session, "new prompt supersedes the snapshot"))
        .await;
    recv(&mut harness.submitted).await;
    drop(blocked);
    let (stale, _) = harness.client.response(2).await;
    assert!(stale.get("error").is_some(), "{stale}");
    harness
        .client
        .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))
        .await;
    recv(&mut harness.ephemeral).await;
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}
