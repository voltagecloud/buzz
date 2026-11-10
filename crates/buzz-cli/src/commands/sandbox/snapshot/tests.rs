use super::*;
use axum::{
    body::Body,
    extract::State,
    routing::{get, post},
    Json, Router,
};
use futures_util::{stream, StreamExt};
use nostr::{EventBuilder, Keys, Kind, Tag};
use std::{
    convert::Infallible,
    sync::{Arc, RwLock},
};

type Media = Option<(Vec<u8>, Option<Arc<tokio::sync::Notify>>)>;

#[derive(Clone)]
struct StateData {
    events: Arc<RwLock<Vec<Event>>>,
    posted: Arc<RwLock<Vec<Event>>>,
    revoke_after_grant: bool,
    media: Media,
}

struct Session {
    owner: Keys,
    agent: Keys,
    relay: Keys,
    requester: Keys,
    scope: Scope,
    pending: wire_snapshot::Pending,
    grant: wire_snapshot::Grant,
    events: Vec<Event>,
}

impl Session {
    fn new(gateway: bool) -> Self {
        let (owner, agent, relay) = (Keys::generate(), Keys::generate(), Keys::generate());
        let requester = if gateway {
            Keys::generate()
        } else {
            owner.clone()
        };
        let channel = Uuid::new_v4();
        let root = EventBuilder::new(Kind::from(9), "sandbox thread")
            .tags([
                Tag::parse(["h", &channel.to_string()]).unwrap(),
                Tag::public_key(agent.public_key()),
            ])
            .sign_with_keys(&owner)
            .unwrap();
        let scope = Scope {
            channel,
            thread: root.id,
            agent: agent.public_key(),
        };
        let request =
            wire_snapshot::request(&requester, scope, Uuid::new_v4(), now() + 600).unwrap();
        let pending = wire_snapshot::Pending::parse(&request, now()).unwrap();
        let approval = wire_snapshot::approve(&owner, &pending, now() + 599).unwrap();
        let grant = wire_snapshot::Grant::parse(&approval, scope, now()).unwrap();
        let mut members = vec![
            owner.public_key(),
            agent.public_key(),
            requester.public_key(),
        ];
        members.sort();
        members.dedup();
        let roster = EventBuilder::new(Kind::from(39002), "")
            .tags(
                std::iter::once(Tag::identifier(channel.to_string())).chain(
                    members
                        .into_iter()
                        .map(|key| Tag::parse(["p", &key.to_hex(), "", "member"]).unwrap()),
                ),
            )
            .sign_with_keys(&relay)
            .unwrap();
        let mut events = vec![root, request, approval, roster];
        if gateway {
            events.push(
                buzz_sdk::gateway::Delegation::grant(requester.public_key(), now() + 600, now())
                    .unwrap()
                    .sign_with_keys(&owner)
                    .unwrap(),
            );
            events.push(
                EventBuilder::new(Kind::Metadata, r#"{"name":"gateway"}"#)
                    .sign_with_keys(&requester)
                    .unwrap(),
            );
        }
        Self {
            owner,
            agent,
            relay,
            requester,
            scope,
            pending,
            grant,
            events,
        }
    }
}

async fn query_handler(
    State(state): State<StateData>,
    Json(filters): Json<Vec<Value>>,
) -> Json<Vec<Event>> {
    let filter = &filters[0];
    assert_eq!(filter["consistency"], "strong");
    let matches = |event: &&Event| {
        let includes = |key: &str, value: Value| {
            filter[key]
                .as_array()
                .is_none_or(|items| items.contains(&value))
        };
        includes("kinds", json!(event.kind.as_u16()))
            && includes("ids", json!(event.id))
            && includes("authors", json!(event.pubkey))
            && ["d", "h", "p"].into_iter().all(|name| {
                filter[format!("#{name}")].as_array().is_none_or(|values| {
                    event.tags.iter().any(|tag| {
                        tag.as_slice().first().is_some_and(|key| key == name)
                            && tag
                                .as_slice()
                                .get(1)
                                .is_some_and(|value| values.contains(&json!(value)))
                    })
                })
            })
    };
    let events = state.events.read().unwrap();
    Json(
        events
            .iter()
            .filter(matches)
            .take(filter["limit"].as_u64().unwrap_or(1) as usize)
            .cloned()
            .collect(),
    )
}

async fn publish_handler(State(state): State<StateData>, Json(event): Json<Event>) -> Json<Value> {
    event.verify().unwrap();
    state.posted.write().unwrap().push(event.clone());
    if !(state.revoke_after_grant && tag(&event, "d").is_some_and(|d| d.ends_with(":grant"))) {
        let mut events = state.events.write().unwrap();
        if let Some(d) = tag(&event, "d") {
            events.retain(|old| {
                old.pubkey != event.pubkey || old.kind != event.kind || tag(old, "d") != Some(d)
            });
        }
        events.push(event.clone());
    } else {
        state
            .events
            .write()
            .unwrap()
            .retain(|old| tag(old, "d") != tag(&event, "d"));
    }
    Json(json!({"event_id":event.id,"accepted":true,"message":"saved"}))
}

async fn media_handler(State(state): State<StateData>) -> Body {
    let (bytes, notify) = state.media.as_ref().unwrap().clone();
    match notify {
        None => Body::from(bytes),
        Some(notify) => Body::from_stream(
            stream::once(async move {
                notify.notify_one();
                Ok::<_, Infallible>(bytes::Bytes::from(bytes[..1].to_vec()))
            })
            .chain(stream::pending()),
        ),
    }
}

async fn fixture(
    session: &Session,
    revoke: bool,
    media: Media,
) -> (BuzzClient, StateData, tokio::task::JoinHandle<()>) {
    let state = StateData {
        events: Arc::new(RwLock::new(session.events.clone())),
        posted: Arc::new(RwLock::new(Vec::new())),
        revoke_after_grant: revoke,
        media,
    };
    let relay = session.relay.public_key();
    let app = Router::new()
        .route("/query", post(query_handler))
        .route("/events", post(publish_handler))
        .route(
            "/info",
            get(move || async move { Json(json!({"pubkey":relay})) }),
        )
        .route("/media/{hash}", get(media_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = BuzzClient::new(
        format!("http://{address}"),
        session.requester.clone(),
        None,
        None,
    )
    .unwrap();
    (client, state, server)
}

#[tokio::test]
async fn snapshot_authority_uses_current_strong_heads_and_live_gateway_membership() {
    let session = Session::new(true);
    let (client, state, server) = fixture(&session, false, None).await;
    authorized(
        &client,
        session.owner.public_key(),
        session.relay.public_key(),
        &session.pending,
        &session.grant,
    )
    .await
    .unwrap();
    for remove in [session.pending.id(), session.grant.id()] {
        state
            .events
            .write()
            .unwrap()
            .retain(|event| event.id != remove);
        assert!(authorized(
            &client,
            session.owner.public_key(),
            session.relay.public_key(),
            &session.pending,
            &session.grant
        )
        .await
        .is_err());
        *state.events.write().unwrap() = session.events.clone();
    }
    state
        .events
        .write()
        .unwrap()
        .retain(|event| event.kind.as_u16() != 30181);
    assert!(authorized(
        &client,
        session.owner.public_key(),
        session.relay.public_key(),
        &session.pending,
        &session.grant
    )
    .await
    .is_err());
    *state.events.write().unwrap() = session.events.clone();
    let roster = session
        .events
        .iter()
        .find(|event| event.kind.as_u16() == 39002)
        .unwrap();
    for missing in [
        session.owner.public_key(),
        session.agent.public_key(),
        session.requester.public_key(),
    ] {
        let removed = EventBuilder::new(roster.kind, "")
            .tags(
                roster
                    .tags
                    .iter()
                    .filter(|tag| tag.as_slice().get(1) != Some(&missing.to_hex()))
                    .cloned(),
            )
            .sign_with_keys(&session.relay)
            .unwrap();
        state
            .events
            .write()
            .unwrap()
            .retain(|event| event.kind.as_u16() != 39002);
        state.events.write().unwrap().push(removed);
        assert!(authorized(
            &client,
            session.owner.public_key(),
            session.relay.public_key(),
            &session.pending,
            &session.grant
        )
        .await
        .is_err());
        *state.events.write().unwrap() = session.events.clone();
    }
    server.abort();
}

#[tokio::test]
async fn snapshot_owner_approval_refuses_gateway_key_and_terminal_request() {
    let session = Session::new(true);
    let (client, state, server) = fixture(&session, false, None).await;
    let error = authorize(&client, session.owner.public_key(), session.pending.id(), 1)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expected owner's primary key"));
    assert!(state.posted.read().unwrap().is_empty());
    server.abort();
    let session = Session::new(false);
    let (client, state, server) = fixture(&session, false, None).await;
    let terminal =
        wire::request(&session.owner, session.scope, Uuid::new_v4(), now() + 600).unwrap();
    state.events.write().unwrap().push(terminal.clone());
    assert!(
        authorize(&client, session.owner.public_key(), terminal.id, 1)
            .await
            .is_err()
    );
    assert!(state.posted.read().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn snapshot_approval_checks_actual_head_after_publication() {
    let session = Session::new(false);
    let (client, state, server) = fixture(&session, true, None).await;
    assert!(
        authorize(&client, session.owner.public_key(), session.pending.id(), 1)
            .await
            .is_err()
    );
    assert_eq!(state.posted.read().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn snapshot_completion_refusal_requires_the_exact_typed_agent_audit() {
    for malformed in [false, true] {
        let session = Session::new(false);
        let (client, state, server) = fixture(&session, false, None).await;
        let event = wire_snapshot::audit(
            &session.agent,
            &session.grant,
            "ended",
            0,
            Some(wire_snapshot::Failure::Scan),
        )
        .unwrap();
        let event = if malformed {
            let mut body: Value = serde_json::from_str(&event.content).unwrap();
            body["status"] = json!("accepted");
            EventBuilder::new(event.kind, body.to_string())
                .tags(event.tags.clone().to_vec())
                .sign_with_keys(&session.agent)
                .unwrap()
        } else {
            event
        };
        state.events.write().unwrap().push(event);
        let error = wait_receipt(
            &client,
            session.owner.public_key(),
            session.relay.public_key(),
            &session.pending,
            &session.grant,
            &CancellationToken::new(),
        )
        .await
        .err()
        .unwrap()
        .to_string();
        if malformed {
            assert!(error.contains("completion does not match approval"));
        } else {
            assert!(error.contains("snapshot creation (Scan)"));
        }
        server.abort();
    }
}

#[tokio::test]
async fn verified_snapshot_download_decrypts_only_while_authorized() {
    let session = Session::new(false);
    let archive = b"local snapshot archive fixture";
    let sealed = wire_snapshot::seal(&session.grant, archive).unwrap();
    let event = wire_snapshot::result(&session.agent, &session.grant, &sealed).unwrap();
    let receipt =
        wire_snapshot::receipt(&session.requester, &session.grant, &event, now()).unwrap();
    let (client, _, server) =
        fixture(&session, false, Some((sealed.ciphertext().to_vec(), None))).await;
    assert_eq!(
        download(
            &client,
            session.owner.public_key(),
            session.relay.public_key(),
            &session.pending,
            &session.grant,
            &receipt,
            &CancellationToken::new()
        )
        .await
        .unwrap(),
        archive
    );
    server.abort();
}

#[tokio::test]
async fn revoked_snapshot_stops_a_stalled_download_without_waiting_for_body_completion() {
    let session = Session::new(false);
    let sealed = wire_snapshot::seal(&session.grant, b"fixture").unwrap();
    let event = wire_snapshot::result(&session.agent, &session.grant, &sealed).unwrap();
    let receipt =
        wire_snapshot::receipt(&session.requester, &session.grant, &event, now()).unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let (client, state, server) = fixture(
        &session,
        false,
        Some((sealed.ciphertext().to_vec(), Some(started.clone()))),
    )
    .await;
    let owner = session.owner.public_key();
    let relay = session.relay.public_key();
    let pending = session.pending;
    let grant = session.grant;
    let revoked = grant.id();
    let job = tokio::spawn(async move {
        download(
            &client,
            owner,
            relay,
            &pending,
            &grant,
            &receipt,
            &CancellationToken::new(),
        )
        .await
    });
    timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    state
        .events
        .write()
        .unwrap()
        .retain(|event| event.id != revoked);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::time::resume();
    assert!(timeout(Duration::from_secs(2), job)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    server.abort();
}

#[tokio::test]
async fn patch_return_posts_existing_diff_kind_in_the_selected_thread_without_changing_source() {
    let session = Session::new(false);
    let (client, state, server) = fixture(&session, false, None).await;
    let directory = tempfile::tempdir().unwrap();
    let patch = directory.path().join("change.diff");
    let content = "--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n";
    std::fs::write(&patch, content).unwrap();
    dispatch(
        SnapshotCmd::PushPatch {
            channel: session.scope.channel,
            thread: session.scope.thread,
            agent: Some(session.scope.agent.to_hex()),
            repo: "https://example.invalid/repo".into(),
            commit: "a".repeat(40),
            file: patch.clone(),
        },
        &client,
    )
    .await
    .unwrap();
    let posted = state.posted.read().unwrap();
    assert_eq!(posted.len(), 1);
    let event = &posted[0];
    assert_eq!(event.kind.as_u16(), 40008);
    assert_eq!(event.content, content);
    assert_eq!(
        tag(event, "h"),
        Some(session.scope.channel.to_string().as_str())
    );
    assert_eq!(
        buzz_core::nip10::parse_thread_markers(&event.tags)
            .resolve()
            .unwrap()
            .0,
        session.scope.thread.to_hex()
    );
    assert_eq!(std::fs::read_to_string(patch).unwrap(), content);
    server.abort();
}

#[test]
fn patch_and_destination_refusals_preserve_existing_local_files() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("change.diff");
    std::fs::write(&file, "x").unwrap();
    assert!(read_patch(&file, "https://token@example.invalid/repo", &"a".repeat(40)).is_err());
    assert!(read_patch(&file, "https://example.invalid/repo", "abcdef0").is_err());
    std::fs::write(&file, vec![b'x'; 60 * 1024 + 1]).unwrap();
    assert!(read_patch(&file, "https://example.invalid/repo", &"a".repeat(40)).is_err());
    std::fs::write(&file, vec![b'x'; 60 * 1024]).unwrap();
    assert_eq!(
        read_patch(&file, "https://example.invalid/repo", &"a".repeat(40))
            .unwrap()
            .len(),
        60 * 1024
    );
    assert!(destination(directory.path(), None).is_err());
    assert!(destination(&directory.path().join("new"), Some(Path::new("/"))).is_ok());
    let other = tempfile::tempdir().unwrap();
    assert!(destination(&directory.path().join("new"), Some(other.path())).is_err());
    #[cfg(unix)]
    {
        let link = directory.path().join("dangling");
        std::os::unix::fs::symlink(directory.path().join("missing"), &link).unwrap();
        assert!(destination(&link, None).is_err());
    }
}
