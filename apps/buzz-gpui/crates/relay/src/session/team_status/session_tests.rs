//! Real session actor, SQLite recovery, signing and loopback HTTP boundary.
use super::*;
use buzz_identity::Identity;
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, Tag, Timestamp};
use std::sync::atomic::AtomicBool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn wait(session: &Session, predicate: impl Fn(State) -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !predicate(session.team_status_state()) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn session_preview_publish_offline_opt_out_and_recovery() {
    let owner = Arc::new(Identity::ephemeral());
    let agent = Identity::ephemeral();
    let relay = Identity::ephemeral();
    let channel = "22222222-2222-4222-8222-222222222222";
    let community_id = "11111111-1111-4111-8111-111111111111";
    let make = |identity: &Identity, kind, tags: Vec<Vec<String>>, content: String| {
        EventBuilder::new(Kind::from_u16(kind), content)
            .tags(tags.into_iter().map(|t| Tag::parse(t).unwrap()))
            .custom_created_at(Timestamp::from_secs(now()))
            .finalize(&Keys::parse(identity.nsec().as_str()).unwrap())
            .unwrap()
    };
    let auth: Vec<String> =
        serde_json::from_str(&owner.attest_agent(&agent.pubkey_hex()).unwrap()).unwrap();
    let root = make(
        &owner,
        kinds::CHAT,
        vec![vec!["h".into(), channel.into()]],
        "Public task".into(),
    );
    let second = make(
        &owner,
        kinds::CHAT,
        vec![vec!["h".into(), channel.into()]],
        "Second public task".into(),
    );
    let root_id = root.id.to_hex();
    let second_id = second.id.to_hex();
    let events = Arc::new(Mutex::new(vec![
        root,
        second,
        make(&agent, kinds::PROFILE, vec![auth], "{}".into()),
        make(
            &relay,
            kinds::CHANNEL_METADATA,
            vec![
                vec!["d".into(), channel.into()],
                vec!["t".into(), "stream".into()],
                vec!["name".into(), "engineering".into()],
                vec!["public".into()],
            ],
            "".into(),
        ),
        make(
            &relay,
            kinds::CHANNEL_ADMINS,
            vec![
                vec!["d".into(), channel.into()],
                vec!["p".into(), owner.pubkey_hex(), "owner".into()],
            ],
            "".into(),
        ),
        make(
            &relay,
            kinds::CHANNEL_MEMBERS,
            vec![
                vec!["d".into(), channel.into()],
                vec!["p".into(), owner.pubkey_hex(), "".into(), "owner".into()],
                vec!["p".into(), agent.pubkey_hex(), "".into(), "bot".into()],
            ],
            "".into(),
        ),
    ]));
    let offline = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin =
        crate::RelayOrigin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let (saved, failed, viewer, authority) = (
        events.clone(),
        offline.clone(),
        owner.pubkey_hex(),
        relay.pubkey_hex(),
    );
    let server = tokio::spawn(async move {
        for _ in 0..512 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (events, offline, viewer, authority) = (
                saved.clone(),
                failed.clone(),
                viewer.clone(),
                authority.clone(),
            );
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let (header_end, length) = loop {
                    let mut part = [0; 4096];
                    let size = socket.read(&mut part).await.unwrap();
                    if size == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&part[..size]);
                    assert!(bytes.len() < 1 << 20);
                    if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|s| {
                                s.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|s| s.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while bytes.len() < header_end + length {
                    let mut part = [0; 4096];
                    let size = socket.read(&mut part).await.unwrap();
                    if size == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&part[..size]);
                }
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..]).unwrap_or(serde_json::Value::Null);
                let is_write = headers.starts_with("POST /events ");
                let failing = is_write && offline.load(Ordering::SeqCst);
                let response = if is_write {
                    let event: Event = serde_json::from_value(body).unwrap();
                    event.verify().unwrap();
                    if event.kind.as_u16() == KIND {
                        assert_eq!(event.tags.len(), 6);
                    }
                    let id = event.id.to_hex();
                    if !failing {
                        events.lock().unwrap().push(event);
                    }
                    json!({"event_id":id,"accepted":!failing,"message":if failing {"offline fixture"} else {""}})
                } else if headers.starts_with("GET / ") {
                    json!({"self":authority,"supported_extensions":["nip-cl"]})
                } else if body[0]["read_state_snapshot"] == 1 {
                    assert!(
                        headers
                            .to_ascii_lowercase()
                            .contains("authorization: nostr ")
                    );
                    json!({"read_state_snapshot":1,"complete":true,"pubkey":viewer,"community_id":community_id,"snapshot_id":"aa".repeat(32),"events":[]})
                } else {
                    let found: Vec<_> = events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|event| {
                            body.as_array().is_some_and(|filters| {
                                filters.iter().any(|f| {
                                    f["kinds"].as_array().is_none_or(|kinds| {
                                        kinds
                                            .iter()
                                            .any(|k| k.as_u64() == Some(event.kind.as_u16() as u64))
                                    }) && f["authors"].as_array().is_none_or(|authors| {
                                        authors
                                            .iter()
                                            .any(|a| a.as_str() == Some(&event.pubkey.to_hex()))
                                    }) && f["ids"].as_array().is_none_or(|ids| {
                                        ids.iter().any(|i| i.as_str() == Some(&event.id.to_hex()))
                                    }) && ["d", "h"].iter().all(|name| {
                                        f[format!("#{name}")].as_array().is_none_or(|values| {
                                            values.iter().any(|v| {
                                                event.tags.iter().any(|t| {
                                                    t.as_slice().first().map(String::as_str)
                                                        == Some(*name)
                                                        && t.as_slice().get(1).map(String::as_str)
                                                            == v.as_str()
                                                })
                                            })
                                        })
                                    })
                                })
                            })
                        })
                        .cloned()
                        .collect();
                    json!(found)
                };
                let body = response.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.sqlite");
    let open = || {
        Session::open(
            &tokio::runtime::Handle::current(),
            origin.clone(),
            owner.clone(),
            Store::open(&path).unwrap(),
        )
    };
    let session = open();
    let mut snapshot = session.snapshot();
    tokio::time::timeout(
        Duration::from_secs(10),
        snapshot.wait_for(|s| s.relay.is_some()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(session.team_status_state().sharing, 0);
    let preview = session
        .preview_team_status(
            &agent.pubkey_hex(),
            channel,
            &root_id,
            Status::Working,
            "Public task",
        )
        .await
        .unwrap();
    assert_eq!(preview.scope().community, community_id);
    assert_eq!(preview.scope().root, root_id);
    assert_eq!(preview.label(), "working: Public task");
    assert!(
        session
            .share_team_status(preview.clone(), false)
            .await
            .is_err()
    );
    session.share_team_status(preview, true).await.unwrap();
    wait(&session, |s| s.pending == 0).await;
    offline.store(true, Ordering::SeqCst);
    let preview = session
        .preview_team_status(
            &agent.pubkey_hex(),
            channel,
            &second_id,
            Status::Blocked,
            "",
        )
        .await
        .unwrap();
    session.share_team_status(preview, true).await.unwrap();
    wait(&session, |s| s.error.is_some()).await;
    session.stop_team_status().unwrap();
    assert_eq!(session.team_status_state().sharing, 0);
    assert_eq!(session.team_status_state().pending, 2);
    drop(session);
    offline.store(false, Ordering::SeqCst);
    let session = open();
    wait(&session, |s| s.pending == 0).await;
    let statuses: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.kind.as_u16() == KIND)
        .cloned()
        .collect();
    assert!(
        statuses.iter().any(|e| e.content.is_empty()
            && e.tags.iter().any(|t| t.as_slice().get(1) == Some(&root_id)))
    );
    assert!(statuses.iter().any(|e| {
        e.content.is_empty()
            && e.tags
                .iter()
                .any(|t| t.as_slice().get(1) == Some(&second_id))
    }));
    drop(session);
    server.abort();
}
