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

/// A root signed by the configured signer that mentions the agent, with
/// extra tags.
fn tagged_root(ids: &Ids, text: &str, extra: &[[&str; 2]]) -> Event {
    root_by(&ids.signer, ids, text, extra)
}

/// A root signed by `author` that mentions the agent, with extra tags.
fn root_by(author: &Keys, ids: &Ids, text: &str, extra: &[[&str; 2]]) -> Event {
    let agent = ids.agent.public_key().to_hex();
    let mut builder =
        buzz_sdk::build_message(ids.channel, text, None, &[&agent], false, &[], &[]).unwrap();
    for parts in extra {
        builder = builder.tag(Tag::parse(*parts).unwrap());
    }
    builder.sign_with_keys(author).unwrap()
}

/// Notifications up to the commands the bridge offers after a session ended.
async fn ended_shown(harness: &mut Harness) -> Vec<Value> {
    let mut notes = Vec::new();
    loop {
        let line = tokio::time::timeout(WAIT, harness.client.stdout.next_line())
            .await
            .expect("ended notice")
            .unwrap()
            .expect("bridge stdout open");
        let note: Value = serde_json::from_str(&line).unwrap();
        let done = note["params"]["update"]["sessionUpdate"] == "available_commands_update";
        notes.push(note);
        if done {
            return notes;
        }
    }
}

/// Session id and listed `cwd` of one `session/list` page, sorted.
async fn listed(harness: &mut Harness, id: u64, params: Value) -> Vec<(String, String)> {
    let (page, _) = harness.client.request(id, "session/list", params).await;
    let mut sessions: Vec<_> = page["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("{page}"))
        .iter()
        .map(|s| {
            (
                s["sessionId"].as_str().unwrap().to_owned(),
                s["cwd"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    sessions.sort();
    sessions
}

/// After the daemon is replaced, Paseo rebuilds its project groups from the
/// relay: a session started in a project lists under that project's
/// directory, everything else under `$HOME/buzz`, and the `cwd` filter
/// selects by the same mapping.
#[tokio::test]
async fn sessions_list_under_their_recorded_project_directory() {
    let ids = Ids::new();
    let home = ids.home.path().to_str().unwrap().to_owned();
    let project = format!("{home}/code/block/buzz");
    let buzz = format!("{home}/buzz");
    std::fs::create_dir_all(&project).unwrap();
    let mut harness = start(&ids, Timing::default());
    harness.relay_tx.send(RelayInbound::Live).await.unwrap();

    // session/new in a branch view's subdirectory records the project.
    let (new, _) = harness
        .client
        .request(
            1,
            "session/new",
            json!({"cwd": format!("{home}/code/block/buzz@feat/x/src"), "mcpServers": []}),
        )
        .await;
    let session = new["result"]["sessionId"].as_str().unwrap().to_owned();
    harness
        .client
        .send(prompt(2, &session, "in a project"))
        .await;
    let in_project = recv(&mut harness.submitted).await;
    let project_tag = vec!["paseo-project".to_owned(), "block/buzz".to_owned()];
    assert_eq!(tag(&in_project, "paseo-project"), [project_tag.as_slice()]);
    // Anywhere else records nothing.
    for (id, cwd) in [(3, "/tmp/elsewhere".to_owned()), (5, home.clone())] {
        let (new, _) = harness
            .client
            .request(id, "session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
        let other = new["result"]["sessionId"].as_str().unwrap().to_owned();
        harness
            .client
            .send(prompt(id + 1, &other, "elsewhere"))
            .await;
        assert!(tag(&recv(&mut harness.submitted).await, "paseo-project").is_empty());
    }

    let untagged = tagged_root(&ids, "no project", &[]);
    let gone = tagged_root(&ids, "deleted clone", &[["paseo-project", "acme/gone"]]);
    // `$HOME/code/../etc` exists, so only the name check keeps it out.
    std::fs::create_dir_all(ids.home.path().join("etc")).unwrap();
    let traversal = tagged_root(&ids, "traversal", &[["paseo-project", "../etc"]]);
    let ambiguous = tagged_root(
        &ids,
        "two projects",
        &[
            ["paseo-project", "block/buzz"],
            ["paseo-project", "acme/gone"],
        ],
    );
    // Another channel member cannot choose the project, even an existing one.
    let member = root_by(
        &Keys::generate(),
        &ids,
        "member thread",
        &[["paseo-project", "block/buzz"]],
    );
    harness.history.lock().unwrap().0 = vec![
        in_project.clone(),
        untagged.clone(),
        gone.clone(),
        traversal.clone(),
        ambiguous.clone(),
        member.clone(),
    ];
    let mut fallback: Vec<_> = [&untagged, &gone, &traversal, &ambiguous, &member]
        .iter()
        .map(|root| (root.id.to_hex(), buzz.clone()))
        .collect();
    fallback.sort();
    let in_project_row = vec![(in_project.id.to_hex(), project.clone())];
    let mut all = fallback.clone();
    all.extend(in_project_row.clone());
    all.sort();
    assert_eq!(listed(&mut harness, 10, json!({})).await, all);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&buzz).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    for (id, cwd, want) in [
        (11, project.clone(), &in_project_row),
        (12, format!("{project}/"), &in_project_row),
        (13, format!("{project}@main"), &in_project_row),
        (14, format!("{project}/crates/x"), &in_project_row),
        (
            15,
            format!("{home}/code/other/../block/buzz"),
            &in_project_row,
        ),
        (16, buzz.clone(), &fallback),
        (17, format!("{buzz}/notes"), &fallback),
        (18, format!("{home}/code/acme/gone"), &vec![]),
        (19, format!("{home}/code/block/other"), &vec![]),
        (20, home.clone(), &vec![]),
        (21, "relative/path".to_owned(), &vec![]),
    ] {
        assert_eq!(
            &listed(&mut harness, id, json!({"cwd": cwd})).await,
            want,
            "{cwd}"
        );
    }

    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();

    // After a restart, a loaded thread keeps its project: `/fresh` after the
    // session ended records it on the new root. A tagged root keeps its own
    // project; an untagged root adopts the client's; a member's tag is
    // ignored.
    let mut restarted = start(&ids, Timing::default());
    restarted.relay_tx.send(RelayInbound::Live).await.unwrap();
    restarted.history.lock().unwrap().0 =
        vec![in_project.clone(), untagged.clone(), member.clone()];
    for (id, root, cwd, want) in [
        (30, &in_project, home.clone(), vec![project_tag.as_slice()]),
        (
            40,
            &untagged,
            format!("{project}@main"),
            vec![project_tag.as_slice()],
        ),
        (50, &member, "/tmp/elsewhere".to_owned(), vec![]),
    ] {
        let loaded_id = root.id.to_hex();
        let (loaded, _) = restarted
            .client
            .request(
                id,
                "session/load",
                json!({"sessionId": loaded_id, "cwd": cwd, "mcpServers": []}),
            )
            .await;
        assert!(loaded.get("result").is_some(), "{loaded}");
        // The manager cannot restore this thread: only `/fresh` is offered.
        restarted
            .inject(thread_message(
                &ids.agent,
                ids.channel,
                &loaded_id,
                NO_CHECKPOINT_NOTICE,
            ))
            .await;
        let notes = ended_shown(&mut restarted).await;
        let shown = texts(&notes).join("");
        assert!(
            shown.contains(&format!("> {NO_CHECKPOINT_NOTICE}"))
                && shown.contains("/fresh")
                && !shown.contains("/recover"),
            "{shown}"
        );
        assert_eq!(advertised(&notes), Some(vec!["fresh".to_owned()]));
        restarted
            .client
            .send(prompt(id + 1, &loaded_id, "/fresh start over"))
            .await;
        let fresh = recv(&mut restarted.submitted).await;
        assert!(fresh.content.starts_with("start over"), "{}", fresh.content);
        assert_eq!(tag(&fresh, "paseo-project"), want, "{cwd}");
    }
    drop(restarted.client);
    restarted.bridge.await.unwrap().unwrap();
}

/// Without a usable `$HOME/buzz`, sessions list under `$HOME` itself.
#[tokio::test]
async fn sessions_list_under_home_when_the_buzz_directory_cannot_be_created() {
    let ids = Ids::new();
    let home = ids.home.path().to_str().unwrap().to_owned();
    std::fs::write(ids.home.path().join("buzz"), "not a directory").unwrap();
    let mut harness = start(&ids, Timing::default());
    let root = tagged_root(&ids, "missing project", &[["paseo-project", "block/buzz"]]);
    harness.history.lock().unwrap().0 = vec![root.clone()];
    let row = vec![(root.id.to_hex(), home.clone())];
    assert_eq!(listed(&mut harness, 1, json!({})).await, row);
    assert_eq!(listed(&mut harness, 2, json!({"cwd": home})).await, row);
    assert!(
        listed(&mut harness, 3, json!({"cwd": format!("{home}/buzz")}))
            .await
            .is_empty()
    );
    drop(harness.client);
    harness.bridge.await.unwrap().unwrap();
}
