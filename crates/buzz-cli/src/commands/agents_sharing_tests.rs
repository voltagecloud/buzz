use super::*;
use axum::{extract::State, routing::post, Json, Router};
use nostr::{Event, EventBuilder, Keys, Kind};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn share_and_unshare_publish_signed_scoped_heads_after_owner_verification() {
    let owner = Keys::generate();
    let agent = Keys::generate();
    let channel = uuid::Uuid::new_v4();
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
    let profile = EventBuilder::new(Kind::Metadata, "{}")
        .tag(buzz_sdk::nip_oa::parse_auth_tag(&auth).unwrap())
        .sign_with_keys(&agent)
        .unwrap();
    let writes = Arc::new(Mutex::new(Vec::<Event>::new()));
    let app = Router::new()
        .route("/query", post(move |Json(filter): Json<Vec<serde_json::Value>>| {
            let profile = profile.clone();
            async move {
                assert_eq!(filter[0]["kinds"], json!([0]));
                assert_eq!(filter[0]["authors"], json!([profile.pubkey]));
                Json(vec![profile])
            }
        }))
        .route("/events", post(|State(writes): State<Arc<Mutex<Vec<Event>>>>, Json(event): Json<Event>| async move {
            assert!(event.verify().is_ok());
            let id = event.id;
            writes.lock().unwrap().push(event);
            Json(json!({"event_id":id, "accepted":true, "message":""}))
        })).with_state(writes.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = BuzzClient::new(url.clone(), owner.clone(), None, None).unwrap();
    for command in [
        AgentsCmd::Share {
            agent: agent.public_key().to_hex(),
            channel,
        },
        AgentsCmd::Unshare {
            agent: agent.public_key().to_hex(),
            channel,
        },
    ] {
        dispatch(command, &client).await.unwrap();
    }
    let denied = BuzzClient::new(url, Keys::generate(), None, None).unwrap();
    assert!(dispatch(
        AgentsCmd::Share {
            agent: agent.public_key().to_hex(),
            channel
        },
        &denied
    )
    .await
    .is_err());
    let events = writes.lock().unwrap();
    assert_eq!(events.len(), 2, "a non-owner must never publish a grant");
    for (event, enabled) in events.iter().zip([true, false]) {
        let grant = buzz_sdk::sharing::Grant::parse(event).unwrap();
        assert_eq!(event.pubkey, owner.public_key());
        assert_eq!(grant.enabled, enabled);
        assert_eq!(grant.channel, channel);
        assert_eq!(grant.agent, agent.public_key());
    }
    server.abort();
}
