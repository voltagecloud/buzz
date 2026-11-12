use super::*;
use axum::{extract::State, routing::post, Json, Router};
use nostr::{EventBuilder, Keys, Kind};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn grant_revoke_and_regrant_replace_heads_and_refuse_agent_credentials() {
    let owner = Keys::generate();
    let gateway = Keys::generate();
    let heads = Arc::new(Mutex::new(Vec::<Event>::new()));
    let app = Router::new()
        .route(
            "/query",
            post(
                |State(heads): State<Arc<Mutex<Vec<Event>>>>,
                 Json(filters): Json<serde_json::Value>| async move {
                    assert_eq!(filters[0]["consistency"], json!("strong"));
                    assert_eq!(
                        filters[0]["kinds"],
                        json!([buzz_sdk::kind::KIND_AGENT_GATEWAY_DELEGATION])
                    );
                    assert_eq!(filters[0]["limit"], json!(1));
                    let coordinate = filters[0]["#d"]
                        .as_array()
                        .filter(|values| values.len() == 1)
                        .map(|values| values[0].clone())
                        .expect("one gateway coordinate");
                    Json(
                        heads
                            .lock()
                            .unwrap()
                            .iter()
                            .rev()
                            // Match an honest relay's author and coordinate filter
                            // for grant heads; metadata is an intentionally
                            // corrupt response below.
                            .find(|head| {
                                head.kind == Kind::Metadata
                                    || (filters[0]["authors"] == json!([head.pubkey])
                                        && head.tags.iter().any(|tag| {
                                            tag.as_slice().first().map(String::as_str) == Some("d")
                                                && tag.as_slice().get(1).map(|v| json!(v))
                                                    == Some(coordinate.clone())
                                        }))
                            })
                            .cloned()
                            .into_iter()
                            .collect::<Vec<_>>(),
                    )
                },
            ),
        )
        .route(
            "/events",
            post(
                |State(heads): State<Arc<Mutex<Vec<Event>>>>, Json(event): Json<Event>| async move {
                    event.verify().unwrap();
                    let id = event.id;
                    heads.lock().unwrap().push(event);
                    Json(json!({"event_id":id,"accepted":true,"message":""}))
                },
            ),
        )
        .with_state(heads.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = BuzzClient::new(url.clone(), owner.clone(), None, None).unwrap();
    let grant = || GatewayCmd::Grant {
        pubkey: gateway.public_key().to_hex(),
        expires: "2w".into(),
    };
    dispatch(grant(), &client).await.unwrap();
    dispatch(
        GatewayCmd::Revoke {
            pubkey: gateway.public_key().to_hex(),
        },
        &client,
    )
    .await
    .unwrap();
    dispatch(grant(), &client).await.unwrap();
    {
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 3);
        for (event, enabled) in heads.iter().zip([true, false, true]) {
            assert_eq!(event.pubkey, owner.public_key());
            assert_eq!(Delegation::parse(event).unwrap().0.enabled, enabled);
        }
        assert!(heads[0].created_at < heads[1].created_at);
        assert!(heads[1].created_at < heads[2].created_at);
    }
    // A newer head for another gateway is outside this coordinate: the
    // replacement time comes from this gateway's own head.
    let other = Keys::generate();
    let newer = heads.lock().unwrap()[2].created_at.as_secs() + 1;
    heads.lock().unwrap().push(
        Delegation::grant(other.public_key(), newer + 3_600, newer)
            .unwrap()
            .sign_with_keys(&owner)
            .unwrap(),
    );
    dispatch(grant(), &client).await.unwrap();
    {
        let heads = heads.lock().unwrap();
        let (renewed, _) = Delegation::parse(heads.last().unwrap()).unwrap();
        assert_eq!(renewed.gateway, gateway.public_key());
        assert!(renewed.enabled);
    }
    for expires in ["0d", "91d", "18446744073709551615d", "forever"] {
        assert!(dispatch(
            GatewayCmd::Grant {
                pubkey: gateway.public_key().to_hex(),
                expires: expires.into()
            },
            &client
        )
        .await
        .is_err());
    }
    assert!(dispatch(
        GatewayCmd::Grant {
            pubkey: owner.public_key().to_hex(),
            expires: "1d".into()
        },
        &client
    )
    .await
    .is_err());
    let agent = Keys::generate();
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
    let agent_client = BuzzClient::new(
        url,
        agent,
        Some(buzz_sdk::nip_oa::parse_auth_tag(&auth).unwrap()),
        None,
    )
    .unwrap();
    assert!(dispatch(grant(), &agent_client).await.is_err());
    // A verified but unrelated head must not influence the replacement time.
    heads.lock().unwrap().clear();
    heads.lock().unwrap().push(
        EventBuilder::new(Kind::Metadata, "{}")
            .sign_with_keys(&owner)
            .unwrap(),
    );
    assert!(dispatch(grant(), &client).await.is_err());
    // A future head must fail visibly instead of accepting a dominated revoke.
    let future = Timestamp::now().as_secs() + 120;
    heads.lock().unwrap().clear();
    heads.lock().unwrap().push(
        Delegation::grant(gateway.public_key(), future + 3_600, future)
            .unwrap()
            .sign_with_keys(&owner)
            .unwrap(),
    );
    assert!(matches!(
        dispatch(
            GatewayCmd::Revoke {
                pubkey: gateway.public_key().to_hex()
            },
            &client
        )
        .await,
        Err(CliError::Conflict(_))
    ));
    assert_eq!(
        heads.lock().unwrap().len(),
        1,
        "no misleading revocation was published"
    );
    server.abort();
}
