use super::*;

fn profile(author: &Keys, name: &str, agent: bool) -> Event {
    let mut builder = EventBuilder::new(Kind::from(0), json!({"display_name":name}).to_string());
    if agent {
        builder = builder.tag(Tag::parse(["auth", "owner", "conditions", "signature"]).unwrap());
    }
    builder.sign_with_keys(author).unwrap()
}

async fn live_text(client: &mut Client) -> String {
    let line = tokio::time::timeout(WAIT, client.stdout.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame: Value = serde_json::from_str(&line).unwrap();
    frame["params"]["update"]["content"]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn teammate_threads_list_load_names_and_cannot_shadow_a_saved_alias() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    let peer = Keys::generate();
    let helper = Keys::generate();
    let alias = Uuid::new_v4().to_string();
    let build = |author: &Keys, text: &str, time: u64| {
        buzz_sdk::build_message(
            ids.channel,
            text,
            None,
            &[&ids.agent.public_key().to_hex()],
            false,
            &[],
            &[],
        )
        .unwrap()
        .tag(Tag::parse(["d", &format!("buzz-acp:{alias}")]).unwrap())
        .custom_created_at(nostr::Timestamp::from(time))
        .sign_with_keys(author)
        .unwrap()
    };
    let owned = build(&ids.signer, "saved opaque handle", 10);
    let teammate = build(&peer, "shared thread from teammate", 11);
    let in_thread = |author: &Keys, text: &str, time: u64| {
        let thread = buzz_sdk::ThreadRef {
            root_event_id: teammate.id,
            parent_event_id: teammate.id,
        };
        buzz_sdk::build_message(ids.channel, text, Some(&thread), &[], false, &[], &[])
            .unwrap()
            .custom_created_at(nostr::Timestamp::from(time))
            .sign_with_keys(author)
            .unwrap()
    };
    harness.history.lock().unwrap().0 = vec![
        owned.clone(),
        teammate.clone(),
        in_thread(&helper, "another agent's note", 12),
        in_thread(&ids.agent, "first answer", 13),
        in_thread(&ids.agent, "second answer", 14),
        profile(&peer, "Taylor", false),
        profile(&helper, "Build helper", true),
    ];
    let (listed, _) = harness.client.request(1, "session/list", json!({})).await;
    let sessions = listed["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0]["sessionId"], teammate.id.to_hex());
    let (loaded, notes) = harness
        .client
        .request(2, "session/load", json!({"sessionId":teammate.id.to_hex()}))
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    // ACP clients join consecutive agent chunks into one message, so replay
    // frames each one as the live path does instead of fusing the text.
    assert_eq!(
        texts(&notes),
        [
            "\n\n[Taylor] shared thread from teammate\n\n",
            "\n\n[Build helper (agent)] another agent's note\n\n",
            "\n\nfirst answer",
            "\n\nsecond answer",
        ]
    );
    let (aliased, notes) = harness
        .client
        .request(3, "session/load", json!({"sessionId":alias}))
        .await;
    assert!(aliased.get("result").is_some(), "{aliased}");
    assert_eq!(texts(&notes), ["saved opaque handle"]);
    // The saved handle and its thread id name one thread. A second handle is
    // refused, so live replies are never split between two sessions.
    let (second, _) = harness
        .client
        .request(30, "session/load", json!({"sessionId":owned.id.to_hex()}))
        .await;
    assert!(
        second["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(&alias)),
        "{second}"
    );
    harness
        .inject(thread_message(
            &peer,
            ids.channel,
            &teammate.id.to_hex(),
            "live shared note",
        ))
        .await;
    assert!(live_text(&mut harness.client)
        .await
        .contains("[Taylor] live shared note"));

    // Neither cached labels nor a blocked profile miss may let the following
    // agent reply overtake the participant's message on the actual ACP wire.
    let newcomer = Keys::generate();
    harness
        .history
        .lock()
        .unwrap()
        .0
        .push(profile(&newcomer, "Avery", false));
    let blocked = harness
        .query_gate
        .clone()
        .acquire_many_owned(4)
        .await
        .unwrap();
    let queries = harness.relay.query_count.load(Ordering::SeqCst);
    for (author, text) in [
        (&peer, "cached participant before answer"),
        (&ids.agent, "answer after cached participant"),
    ] {
        harness
            .inject(thread_message(
                author,
                ids.channel,
                &teammate.id.to_hex(),
                text,
            ))
            .await;
    }
    assert!(live_text(&mut harness.client)
        .await
        .contains("[Taylor] cached participant before answer"));
    assert_eq!(
        live_text(&mut harness.client).await,
        "answer after cached participant"
    );
    assert_eq!(harness.relay.query_count.load(Ordering::SeqCst), queries);
    for (author, text) in [
        (&newcomer, "uncached participant before answer"),
        (&ids.agent, "answer after uncached participant"),
    ] {
        harness
            .inject(thread_message(
                author,
                ids.channel,
                &teammate.id.to_hex(),
                text,
            ))
            .await;
    }
    assert!(live_text(&mut harness.client).await.contains(&format!(
        "[{}] uncached participant before answer",
        super::super::names::fallback(newcomer.public_key())
    )));
    assert_eq!(
        live_text(&mut harness.client).await,
        "answer after uncached participant"
    );
    tokio::time::timeout(WAIT, async {
        while harness.relay.query_count.load(Ordering::SeqCst) == queries {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(blocked);
    // The semaphore's FIFO queue puts this acquisition after the already
    // blocked warm-up. Its release lets that task finish without a time sleep.
    let warmed = tokio::time::timeout(WAIT, harness.query_gate.clone().acquire_many_owned(4))
        .await
        .unwrap()
        .unwrap();
    harness
        .inject(thread_message(
            &newcomer,
            ids.channel,
            &teammate.id.to_hex(),
            "next message uses the warmed profile",
        ))
        .await;
    assert!(live_text(&mut harness.client)
        .await
        .contains("[Avery] next message uses the warmed profile"));
    drop(warmed);
    let (_, extra) = harness.client.request(4, "session/new", json!({})).await;
    assert!(extra.is_empty(), "warm-up must not repeat delivered text");
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

#[tokio::test]
async fn slow_peer_profile_lookup_keeps_cancel_responsive() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    harness.relay_tx.send(RelayInbound::Live).await.unwrap();
    let (new, _) = harness.client.request(1, "session/new", json!({})).await;
    let session = new["result"]["sessionId"].as_str().unwrap();
    harness.client.send(prompt(2, session, "run")).await;
    let root = recv(&mut harness.submitted).await;
    harness.consume_query_permits.store(true, Ordering::SeqCst);
    let blocked = harness
        .query_gate
        .clone()
        .acquire_many_owned(4)
        .await
        .unwrap();
    harness
        .inject(thread_message(
            &Keys::generate(),
            ids.channel,
            &root.id.to_hex(),
            "peer during prompt",
        ))
        .await;
    tokio::time::timeout(WAIT, harness.query_entered.notified())
        .await
        .unwrap();
    harness
        .client
        .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}))
        .await;
    recv(&mut harness.ephemeral).await;
    drop(blocked);
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}

#[tokio::test]
async fn opaque_handle_scan_excludes_other_authors_before_its_candidate_limit() {
    let ids = Ids::new();
    let mut harness = start(&ids, Timing::default());
    let alias = Uuid::new_v4().to_string();
    let build = |author: &Keys, text: &str, time: u64| {
        buzz_sdk::build_message(
            ids.channel,
            text,
            None,
            &[&ids.agent.public_key().to_hex()],
            false,
            &[],
            &[],
        )
        .unwrap()
        .tag(Tag::parse(["d", &format!("buzz-acp:{alias}")]).unwrap())
        .custom_created_at(nostr::Timestamp::from(time))
        .sign_with_keys(author)
        .unwrap()
    };
    let owned = build(&ids.signer, "my persisted handle", 10);
    let peer = Keys::generate();
    let mut events = vec![owned];
    events.extend((0..1000).map(|index| build(&peer, &format!("copied alias {index}"), 11)));
    harness.history.lock().unwrap().0 = events;
    let (loaded, notes) = harness
        .client
        .request(1, "session/load", json!({"sessionId":alias}))
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert_eq!(texts(&notes), ["my persisted handle"]);
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}
