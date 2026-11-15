use super::*;
use std::collections::HashSet;

pub(super) fn roster(scope: &Scope, channel: Uuid, member: bool) -> Event {
    let mut tags = vec![Tag::parse(["d", &channel.to_string()]).unwrap()];
    if member {
        tags.push(Tag::parse(["p", &scope.owner.to_hex(), "", "member"]).unwrap());
    }
    EventBuilder::new(Kind::Custom(39002), "")
        .tags(tags)
        .allow_self_tagging()
        .sign_with_keys(&scope.signer)
        .unwrap()
}

fn root(ids: &Ids, channel: Uuid, time: u64) -> Event {
    buzz_sdk::build_message(
        channel,
        &format!("thread at {time}"),
        None,
        &[&ids.agent.public_key().to_hex()],
        false,
        &[],
        &[],
    )
    .unwrap()
    .custom_created_at(nostr::Timestamp::from(time))
    .sign_with_keys(&ids.signer)
    .unwrap()
}

/// Production list/load/prompt routes a restored conversation independently
/// from the default channel, and excludes departed and global participation.
#[tokio::test]
async fn restored_threads_keep_their_channel_and_new_sessions_use_the_default() {
    let ids = Ids::new();
    let b = Uuid::new_v4();
    let departed = Uuid::new_v4();
    let mut h = start(&ids, Timing::default());
    h.relay_tx.send(RelayInbound::Live).await.unwrap();
    let a_root = root(&ids, ids.channel, 100);
    let b_root = root(&ids, b, 101);
    let left_root = root(&ids, departed, 102);
    let global = EventBuilder::new(Kind::Custom(9), "global participation")
        .tags([Tag::public_key(ids.agent.public_key())])
        .sign_with_keys(&ids.signer)
        .unwrap();
    h.history.lock().unwrap().0 = vec![
        a_root.clone(),
        b_root.clone(),
        left_root.clone(),
        global,
        roster(&ids.scope(), b, true),
        roster(&ids.scope(), departed, false),
    ];
    let (list, _) = h.client.request(1, "session/list", json!({})).await;
    let sessions = list["result"]["sessions"].as_array().unwrap();
    let listed: HashSet<_> = sessions
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        listed,
        HashSet::from([a_root.id.to_hex(), b_root.id.to_hex()])
    );
    let (refused, _) = h
        .client
        .request(
            2,
            "session/load",
            json!({"sessionId":left_root.id.to_hex()}),
        )
        .await;
    assert!(refused.get("error").is_some(), "{refused}");
    let session = b_root.id.to_hex();
    let (loaded, _) = h
        .client
        .request(3, "session/load", json!({"sessionId":session}))
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    h.client.send(prompt(4, &session, "continue in B")).await;
    let posted = recv(&mut h.submitted).await;
    assert_eq!(tag(&posted, "h")[0][1], b.to_string());
    assert!(tag(&posted, "e").iter().any(|t| t[1] == session));
    h.inject(ids.telemetry(&observer(
        "turn_started",
        Some(b),
        Some("b-turn"),
        json!({"triggeringEventIds":[posted.id.to_hex()]}),
    )))
    .await;
    h.inject(ids.telemetry(&observer(
        "acp_read",
        Some(ids.channel),
        Some("b-turn"),
        chunk("WRONG CHANNEL TELEMETRY"),
    )))
    .await;
    h.inject(ids.telemetry(&observer(
        "acp_read",
        Some(b),
        Some("b-turn"),
        chunk("correct B telemetry"),
    )))
    .await;
    h.inject(thread_message(
        &ids.agent,
        ids.channel,
        &session,
        "WRONG CHANNEL REPLY",
    ))
    .await;
    h.inject(thread_message(&ids.agent, b, &session, "correct B reply"))
        .await;
    h.inject(ids.telemetry(&observer(
        "turn_completed",
        Some(b),
        Some("b-turn"),
        json!({}),
    )))
    .await;
    let (done, notes) = h.client.response(4).await;
    assert!(done.get("result").is_some(), "{done}");
    let text = texts(&notes).join(" ");
    assert!(text.contains("correct B telemetry"), "{text}");
    assert!(text.contains("correct B reply"), "{text}");
    assert!(!text.contains("WRONG CHANNEL"), "{text}");
    let (new, _) = h.client.request(5, "session/new", json!({})).await;
    let new = new["result"]["sessionId"].as_str().unwrap();
    h.client.send(prompt(6, new, "new in C")).await;
    let posted = recv(&mut h.submitted).await;
    assert_eq!(tag(&posted, "h")[0][1], ids.channel.to_string());
    assert!(tag(&posted, "e").is_empty());
    h.bridge.abort();
}

/// More than 32 cold membership reads must return a resumable cursor before
/// the first unchecked root. Owner exclusions do not turn a short page into EOF.
#[tokio::test]
async fn cold_membership_budget_pages_without_skipping_threads() {
    let ids = Ids::new();
    let mut h = start(&ids, Timing::default());
    let mut expected = HashSet::new();
    let mut events = Vec::new();
    for i in 0..80 {
        let channel = Uuid::new_v4();
        let member = i < 40;
        let root = root(&ids, channel, 100 + i);
        if member {
            expected.insert(root.id.to_hex());
        }
        events.push(root);
        events.push(roster(&ids.scope(), channel, member));
    }
    h.history.lock().unwrap().0 = events;
    let mut cursor = None;
    let mut found = HashSet::new();
    let mut saw_budget_page = false;
    for page in 0..10 {
        let params = cursor
            .map(|cursor| json!({"cursor":cursor}))
            .unwrap_or_else(|| json!({}));
        let (response, _) = h.client.request(page + 1, "session/list", params).await;
        assert!(response.get("error").is_none(), "{response}");
        let sessions = response["result"]["sessions"].as_array().unwrap();
        if sessions.is_empty() && response["result"]["nextCursor"].is_string() {
            saw_budget_page = true;
        }
        for session in sessions {
            assert!(
                found.insert(session["sessionId"].as_str().unwrap().to_owned()),
                "duplicate session"
            );
        }
        cursor = response["result"]["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none(), "listing failed to finish");
    assert!(saw_budget_page, "fixture never exhausted membership budget");
    assert_eq!(found, expected);
    h.bridge.abort();
}

/// A real authenticated WebSocket adds B without replaying retained A.
/// A B reply races catch-up. Replay and live copies are
/// delivered once through the production bridge loop.
#[tokio::test]
async fn replacing_the_subscription_delivers_a_racing_reply_once_and_evicts_channels() {
    use crate::commands::acp_bridge::{stream_channels, DEADLINES, THREAD_SUB};
    use axum::{
        extract::ws::{Message, WebSocket, WebSocketUpgrade},
        routing::get,
        Router,
    };
    async fn read(socket: &mut WebSocket) -> Value {
        loop {
            if let Message::Text(text) = socket.recv().await.unwrap().unwrap() {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }
    async fn send(socket: &mut WebSocket, value: Value) {
        socket
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }
    let ids = Ids::new();
    let b = Uuid::new_v4();
    let mut h = start(&ids, Timing::default());
    let b_root = root(&ids, b, nostr::Timestamp::now().as_secs());
    let session = b_root.id.to_hex();
    let race = thread_message(&ids.agent, b, &session, "raced B reply");
    let marker = thread_message(&ids.agent, b, &session, "replacement complete");
    let a_root = root(&ids, ids.channel, nostr::Timestamp::now().as_secs());
    let a_session = a_root.id.to_hex();
    let old_reply = thread_message(&ids.agent, ids.channel, &a_session, "original A reply");
    h.history.lock().unwrap().0 = vec![a_root, b_root, roster(&ids.scope(), b, true)];
    h.client
        .request(1, "session/load", json!({"sessionId":a_session}))
        .await;
    h.inject(old_reply.clone()).await;
    // Expire the reply from the 4,096-ID cache with unrelated traffic.
    for i in 0..4097 {
        let noise = EventBuilder::new(Kind::Custom(9), format!("noise {i}"))
            .tags([Tag::parse(["h", &ids.channel.to_string()]).unwrap()])
            .sign_with_keys(&ids.signer)
            .unwrap();
        h.inject(noise).await;
    }
    let (_, initial_notes) = h.client.request(1000, "initialize", json!({})).await;
    assert_eq!(
        texts(&initial_notes)
            .iter()
            .filter(|text| text.contains("original A reply"))
            .count(),
        1
    );
    let channels = h.relay.channels.subscribe();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (filters_tx, mut filters_rx) = mpsc::unbounded_channel();
    let app = Router::new().route(
        "/",
        get(move |upgrade: WebSocketUpgrade| {
            let (race, marker, old_reply, filters_tx) = (
                race.clone(),
                marker.clone(),
                old_reply.clone(),
                filters_tx.clone(),
            );
            async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    send(&mut socket, json!(["AUTH", "challenge"])).await;
                    let auth = read(&mut socket).await;
                    send(&mut socket, json!(["OK", auth[1]["id"], true, ""])).await;
                    loop {
                        let request = read(&mut socket).await;
                        if request[0] != "REQ" {
                            continue;
                        }
                        let replacement = request[1]
                            .as_str()
                            .is_some_and(|id| id.starts_with(THREAD_SUB))
                            && request[2]["#h"]
                                .as_array()
                                .unwrap()
                                .contains(&json!(b.to_string()));
                        if request[1]
                            .as_str()
                            .is_some_and(|id| id.starts_with(THREAD_SUB))
                        {
                            filters_tx.send(request[2].clone()).unwrap();
                        }
                        if replacement {
                            // A relay would replay A too if the replacement includes it.
                            if request[2]["#h"].as_array().unwrap().contains(&json!(
                                old_reply
                                    .tags
                                    .iter()
                                    .find(|tag| tag.as_slice()[0] == "h")
                                    .unwrap()
                                    .as_slice()[1]
                            )) {
                                send(&mut socket, json!(["EVENT", request[1], old_reply])).await;
                            }
                            send(&mut socket, json!(["EVENT", request[1], race])).await;
                        }
                        send(&mut socket, json!(["EOSE", request[1]])).await;
                        if replacement {
                            send(&mut socket, json!(["EVENT", request[1], race])).await;
                            send(&mut socket, json!(["EVENT", request[1], marker])).await;
                        }
                    }
                })
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (scope, relay_tx) = (ids.scope(), h.relay_tx.clone());
    let stream = tokio::spawn(async move {
        stream_channels(&url, &scope, None, 0, &relay_tx, DEADLINES, channels).await
    });
    let initial = recv(&mut filters_rx).await;
    assert_eq!(initial["#h"], json!([ids.channel.to_string()]));
    let (loaded, mut notes) = h
        .client
        .request(2, "session/load", json!({"sessionId":session}))
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    let replacement = recv(&mut filters_rx).await;
    assert_eq!(replacement["#h"], json!([b.to_string()]));
    assert!(replacement["since"].as_u64().unwrap() <= nostr::Timestamp::now().as_secs() - 100);
    while !texts(&notes)
        .iter()
        .any(|text| text.contains("replacement complete"))
    {
        let line = tokio::time::timeout(WAIT, h.client.stdout.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        notes.push(serde_json::from_str(&line).unwrap());
    }
    assert!(!texts(&notes)
        .iter()
        .any(|text| text.contains("original A reply")));
    assert_eq!(
        texts(&notes)
            .iter()
            .filter(|text| text.contains("raced B reply"))
            .count(),
        1
    );
    for id in 3..67 {
        h.client.request(id, "session/new", json!({})).await;
    }
    h.client.request(67, "initialize", json!({})).await;
    assert_eq!(
        h.relay
            .channels
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![ids.channel]
    );
    stream.abort();
    server.abort();
    h.bridge.abort();
}

#[tokio::test(start_paused = true)]
async fn membership_cache_expires_and_load_checks_the_current_roster() {
    let ids = Ids::new();
    let mut h = start(&ids, Timing::default());
    let thread = root(&ids, ids.channel, 100);
    h.history.lock().unwrap().0 = vec![thread.clone()];
    let (first, _) = h.client.request(1, "session/list", json!({})).await;
    assert_eq!(first["result"]["sessions"].as_array().unwrap().len(), 1);
    h.history
        .lock()
        .unwrap()
        .0
        .push(roster(&ids.scope(), ids.channel, false));
    let (load, _) = h
        .client
        .request(2, "session/load", json!({"sessionId":thread.id.to_hex()}))
        .await;
    assert!(
        load.get("error").is_some(),
        "loading must not reuse a stale positive roster"
    );
    tokio::time::advance(Duration::from_secs(31)).await;
    let (list, _) = h.client.request(3, "session/list", json!({})).await;
    assert!(list["result"]["sessions"].as_array().unwrap().is_empty());
    h.bridge.abort();
}

/// Wrong-channel witnesses must not exhaust the prefix budget on resumption.
#[tokio::test]
async fn discovery_resumes_past_a_full_page_of_wrong_channel_participation() {
    let ids = Ids::new();
    let peer = Keys::generate();
    let other_channel = Uuid::new_v4();
    let root = buzz_sdk::build_message(ids.channel, "ordinary root", None, &[], false, &[], &[])
        .unwrap()
        .custom_created_at(nostr::Timestamp::from(1))
        .sign_with_keys(&peer)
        .unwrap();
    let root_id = root.id.to_hex();
    let valid = buzz_sdk::build_message(
        ids.channel,
        "valid participation",
        Some(&ThreadRef {
            root_event_id: root.id,
            parent_event_id: root.id,
        }),
        &[],
        false,
        &[],
        &[],
    )
    .unwrap()
    .custom_created_at(nostr::Timestamp::from(2))
    .sign_with_keys(&ids.agent)
    .unwrap();
    let mut events = vec![root, valid];
    for i in 0..1000 {
        events.push(
            buzz_sdk::build_message(
                other_channel,
                "wrong channel",
                Some(&ThreadRef {
                    root_event_id: EventId::from_hex(&root_id).unwrap(),
                    parent_event_id: EventId::from_hex(&root_id).unwrap(),
                }),
                &[],
                false,
                &[],
                &[],
            )
            .unwrap()
            .custom_created_at(nostr::Timestamp::from(10 + i))
            .sign_with_keys(&ids.agent)
            .unwrap(),
        );
    }
    let mut h = start(&ids, Timing::default());
    h.history.lock().unwrap().0 = events;
    let (first, _) = h.client.request(1, "session/list", json!({})).await;
    assert!(first["result"]["sessions"].as_array().unwrap().is_empty());
    let cursor = first["result"]["nextCursor"].as_str().unwrap();
    let (second, _) = h
        .client
        .request(2, "session/list", json!({"cursor":cursor}))
        .await;
    assert!(second.get("error").is_none(), "{second}");
    assert_eq!(second["result"]["sessions"][0]["sessionId"], root_id);
    h.bridge.abort();
}
