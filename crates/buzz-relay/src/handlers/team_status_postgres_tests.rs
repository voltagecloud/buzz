//! Team status journeys through real ingestion and scoped database reads.
use super::group_state_postgres_tests::{community, create_channel, req_event_ids, test_conn};
use super::ingest::{ingest_event, HttpAuthMethod, IngestAuth};
use buzz_auth::Scope;
use buzz_core::{team_status, CommunityId};
use buzz_db::EventQuery;
use nostr::{Event, EventBuilder, EventId, Keys, Kind, PublicKey, Tag, Timestamp};
use uuid::Uuid;

fn auth(keys: &Keys) -> IngestAuth {
    IngestAuth::Http {
        pubkey: keys.public_key(),
        scopes: vec![
            Scope::UsersWrite,
            Scope::MessagesWrite,
            Scope::AdminChannels,
        ],
        auth_method: HttpAuthMethod::Nip98,
    }
}
fn event(keys: &Keys, kind: u16, content: &str, tags: Vec<Vec<String>>, created: u64) -> Event {
    EventBuilder::new(Kind::Custom(kind), content)
        .custom_created_at(Timestamp::from(created))
        .tags(tags.into_iter().map(|tag| Tag::parse(tag).unwrap()))
        .sign_with_keys(keys)
        .unwrap()
}
fn status(
    owner: &Keys,
    agent: PublicKey,
    scope: (CommunityId, Uuid, EventId),
    content: &str,
    created: u64,
) -> Event {
    let (community, channel, root) = scope;
    event(
        owner,
        30623,
        content,
        vec![
            vec![
                "d".into(),
                team_status::address(community, agent, channel, root),
            ],
            vec!["community".into(), community.to_string()],
            vec!["h".into(), channel.to_string()],
            vec!["p".into(), agent.to_hex()],
            vec!["e".into(), root.to_hex(), "".into(), "root".into()],
            vec!["expiration".into(), (created + 900).to_string()],
        ],
        created,
    )
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn owner_status_is_scoped_clear_dominates_and_membership_revocation_hides_heads() {
    let state = crate::api::bridge::postgres_tests::bridge_handler_test_state()
        .await
        .expect("local Postgres and Redis");
    let tenant = community(&state, "team-status").await;
    let owner = Keys::generate();
    let agent = Keys::generate();
    let viewer = Keys::generate();
    let outsider = Keys::generate();
    for key in [&owner, &agent, &viewer, &outsider] {
        state
            .db
            .ensure_user(tenant.community(), &key.public_key().to_bytes())
            .await
            .unwrap();
    }
    state
        .db
        .set_agent_owner(
            tenant.community(),
            &agent.public_key().to_bytes(),
            &owner.public_key().to_bytes(),
        )
        .await
        .unwrap();
    let channel = create_channel(&state, &tenant, &owner, &[&["visibility", "private"]]).await;
    let now = Timestamp::now().as_secs();
    for key in [&agent, &viewer] {
        let add = event(
            &owner,
            9000,
            "",
            vec![
                vec!["h".into(), channel.to_string()],
                vec!["p".into(), key.public_key().to_hex()],
            ],
            now,
        );
        ingest_event(&state, &tenant, add, auth(&owner))
            .await
            .unwrap();
    }
    let root = event(
        &owner,
        9,
        "Publicly approved task",
        vec![vec!["h".into(), channel.to_string()]],
        now,
    );
    ingest_event(&state, &tenant, root.clone(), auth(&owner))
        .await
        .unwrap();
    let working = status(
        &owner,
        agent.public_key(),
        (tenant.community(), channel, root.id),
        r#"{"v":1,"label":"working"}"#,
        now,
    );
    assert!(
        ingest_event(&state, &tenant, working.clone(), auth(&owner))
            .await
            .unwrap()
            .accepted
    );
    let attributed = event(
        &owner,
        9,
        "Workflow attribution is not a mention",
        vec![
            vec!["h".into(), channel.to_string()],
            vec!["buzz:workflow".into(), "true".into()],
            vec![
                "p".into(),
                agent.public_key().to_hex(),
                "".into(),
                "owner".into(),
            ],
        ],
        now,
    );
    ingest_event(&state, &tenant, attributed.clone(), auth(&owner))
        .await
        .unwrap();
    // Exercise transport gates as a teammate whose key is not the subject.
    for reader in [&owner, &viewer] {
        for filter in [
            serde_json::json!({"kinds":[30623], "#h":[channel.to_string()]}),
            serde_json::json!({"kinds":[30623], "#h":[channel.to_string()], "#p":[agent.public_key().to_hex()]}),
            serde_json::json!({"ids":[working.id.to_hex()]}),
            serde_json::json!({"ids":[working.id.to_hex()], "#p":[agent.public_key().to_hex()]}),
            serde_json::json!({"ids":[working.id.to_hex(), attributed.id.to_hex()], "#p":[agent.public_key().to_hex()]}),
        ] {
            assert_eq!(
                req_event_ids(&state, &tenant, reader, filter.clone()).await,
                vec![working.id.to_hex()]
            );
            let (conn, mut frames) = test_conn(&tenant, reader);
            super::count::handle_count(
                "status".into(),
                vec![serde_json::from_value(filter.clone()).unwrap()],
                conn,
                state.clone(),
            )
            .await;
            let axum::extract::ws::Message::Text(frame) = frames.try_recv().unwrap() else {
                panic!("expected COUNT");
            };
            let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(value[0], "COUNT");
            assert_eq!(value[2]["count"], 1);
            let response = post_read(
                state.clone(),
                tenant.host(),
                reader,
                "/query",
                filter.clone(),
            )
            .await;
            assert_eq!(response[0]["id"], working.id.to_hex());
            assert_eq!(
                post_read(state.clone(), tenant.host(), reader, "/count", filter).await["count"],
                1
            );
        }
    }
    let outside_filter = serde_json::json!({"ids":[working.id.to_hex()]});
    assert!(
        req_event_ids(&state, &tenant, &outsider, outside_filter.clone())
            .await
            .is_empty()
    );
    assert_eq!(
        post_read(
            state.clone(),
            tenant.host(),
            &outsider,
            "/query",
            outside_filter.clone()
        )
        .await,
        serde_json::json!([])
    );
    assert_eq!(
        post_read(
            state.clone(),
            tenant.host(),
            &outsider,
            "/count",
            outside_filter
        )
        .await["count"],
        0
    );
    let query = |reader: &Keys| EventQuery {
        kinds: Some(vec![30623]),
        channel_id: Some(channel),
        team_status_reader: Some(reader.public_key().to_bytes().to_vec()),
        ..EventQuery::for_community(tenant.community())
    };
    assert_eq!(
        state.db.query_events(&query(&viewer)).await.unwrap().len(),
        1
    );
    assert_eq!(state.db.count_events(&query(&viewer)).await.unwrap(), 1);
    for reader in [&outsider] {
        assert!(state
            .db
            .query_events(&query(reader))
            .await
            .unwrap()
            .is_empty());
        let mut by_id = query(reader);
        by_id.kinds = None;
        by_id.ids = Some(vec![working.id.as_bytes().to_vec()]);
        assert!(state.db.query_events(&by_id).await.unwrap().is_empty());
        assert_eq!(state.db.count_events(&by_id).await.unwrap(), 0);
    }
    let forged = status(
        &viewer,
        agent.public_key(),
        (tenant.community(), channel, root.id),
        r#"{"v":1,"label":"done"}"#,
        now + 1,
    );
    assert!(ingest_event(&state, &tenant, forged, auth(&viewer))
        .await
        .is_err());
    let cross_root = status(
        &owner,
        agent.public_key(),
        (
            tenant.community(),
            channel,
            EventId::from_byte_array([42; 32]),
        ),
        "",
        now + 1,
    );
    assert!(ingest_event(&state, &tenant, cross_root, auth(&owner))
        .await
        .is_err());
    let other = community(&state, "team-status-other").await;
    assert!(ingest_event(&state, &other, working.clone(), auth(&owner))
        .await
        .is_err());
    let (live, mut live_frames) = test_conn(&tenant, &viewer);
    state.conn_manager.register(
        live.conn_id,
        live.send_tx.clone(),
        live.ctrl_tx.clone(),
        live.terminal_ctrl_tx.clone(),
        None,
        live.cancel.clone(),
        tenant.community(),
        live.backpressure_count.clone(),
        live.subscriptions.clone(),
        3,
        live.community_control.clone(),
    );
    state
        .conn_manager
        .set_authenticated_pubkey(live.conn_id, viewer.public_key().to_bytes().to_vec());
    super::req::handle_req(
        "live-status".into(),
        vec![serde_json::from_value(
            serde_json::json!({"kinds":[30623],"#h":[channel.to_string()]}),
        )
        .unwrap()],
        vec![None],
        live.clone(),
        state.clone(),
    )
    .await;
    while live_frames.try_recv().is_ok() {}
    let clear = status(
        &owner,
        agent.public_key(),
        (tenant.community(), channel, root.id),
        "",
        now + 2,
    );
    ingest_event(&state, &tenant, clear.clone(), auth(&owner))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let axum::extract::ws::Message::Text(frame) =
                live_frames.recv().await.expect("live frame")
            else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
            if value[0] == "EVENT" && value[2]["id"] == clear.id.to_hex() {
                break;
            }
        }
    })
    .await
    .expect("clear reaches authorized live subscriber");
    ingest_event(&state, &tenant, working.clone(), auth(&owner))
        .await
        .unwrap();
    let heads = state.db.query_events(&query(&viewer)).await.unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].event.id, clear.id);
    assert!(heads[0].event.content.is_empty());
    let delete = event(
        &owner,
        5,
        "",
        vec![vec!["e".into(), clear.id.to_hex()]],
        now + 3,
    );
    assert!(ingest_event(&state, &tenant, delete, auth(&owner))
        .await
        .is_err());
    // Ordinary feed queries never include team status even for an authorized reader.
    let mut ordinary = query(&viewer);
    ordinary.kinds = None;
    assert!(state
        .db
        .query_events(&ordinary)
        .await
        .unwrap()
        .iter()
        .all(|e| e.event.kind.as_u16() != 30623));
    let remove = event(
        &owner,
        9001,
        "",
        vec![
            vec!["h".into(), channel.to_string()],
            vec!["p".into(), viewer.public_key().to_hex()],
        ],
        now + 4,
    );
    ingest_event(&state, &tenant, remove, auth(&owner))
        .await
        .unwrap();
    assert!(state
        .db
        .query_events(&query(&viewer))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        post_read(
            state.clone(),
            tenant.host(),
            &viewer,
            "/query",
            serde_json::json!({"ids":[clear.id.to_hex()]})
        )
        .await,
        serde_json::json!([])
    );
    // Replacing one address still consumes the per-tuple publication budget.
    for offset in 5..15 {
        let next = status(
            &owner,
            agent.public_key(),
            (tenant.community(), channel, root.id),
            r#"{"v":1,"label":"waiting"}"#,
            now + offset,
        );
        assert!(
            ingest_event(&state, &tenant, next, auth(&owner))
                .await
                .unwrap()
                .accepted
        );
    }
    let excess = status(
        &owner,
        agent.public_key(),
        (tenant.community(), channel, root.id),
        "",
        now + 15,
    );
    let error = ingest_event(&state, &tenant, excess, auth(&owner))
        .await
        .err()
        .expect("rate limit rejection");
    assert!(format!("{error:?}").contains("publication rate exceeded"));
}

async fn post_read(
    state: std::sync::Arc<crate::state::AppState>,
    host: &str,
    reader: &Keys,
    path: &str,
    filter: serde_json::Value,
) -> serde_json::Value {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let response = crate::router::build_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("host", host)
                .header("x-pubkey", reader.public_key().to_hex())
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!([filter]).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}
