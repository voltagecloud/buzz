//! Production bridge + HTTP client tests; the HTTP fixture verifies signed
//! Blossom/NIP-98 requests. Live-relay acceptance is a separate runtime check.

use super::*;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post, put},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use sha2::{Digest, Sha256};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[derive(Clone)]
struct HttpRelay {
    url: String,
    gateway: PublicKey,
    agent: PublicKey,
    channel: Uuid,
    fail_upload: bool,
    submitted: mpsc::UnboundedSender<Event>,
    blobs: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

fn signed(headers: &HeaderMap) -> Event {
    let auth = headers["authorization"]
        .to_str()
        .unwrap()
        .strip_prefix("Nostr ")
        .unwrap();
    let raw = URL_SAFE_NO_PAD
        .decode(auth)
        .or_else(|_| STANDARD.decode(auth))
        .unwrap();
    let event: Event = serde_json::from_slice(&raw).unwrap();
    event.verify().unwrap();
    event
}

async fn upload(
    State(state): State<HttpRelay>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, StatusCode> {
    let auth = signed(&headers);
    assert_eq!(auth.pubkey, state.gateway);
    assert_eq!(auth.kind.as_u16(), 24242);
    assert_eq!(tag(&auth, "t")[0][1], "upload");
    let hash = hex::encode(Sha256::digest(&body));
    assert_eq!(tag(&auth, "x")[0][1], hash);
    if state.fail_upload {
        return Err(StatusCode::BAD_REQUEST);
    }
    state
        .blobs
        .lock()
        .unwrap()
        .insert(hash.clone(), body.to_vec());
    Ok(Json(
        json!({"url":format!("{}/media/{hash}.png",state.url),"sha256":hash,"size":body.len(),"type":"image/png","uploaded":nostr::Timestamp::now().as_secs()}),
    ))
}

async fn query(State(state): State<HttpRelay>, headers: HeaderMap) -> Json<Value> {
    assert_eq!(signed(&headers).pubkey, state.gateway);
    Json(
        json!([{"kind":39002,"tags":[["d",state.channel.to_string()],["p",state.gateway.to_hex()],["p",state.agent.to_hex()]]}]),
    )
}

async fn submit(
    State(state): State<HttpRelay>,
    headers: HeaderMap,
    Json(event): Json<Event>,
) -> Json<Value> {
    assert_eq!(signed(&headers).pubkey, state.gateway);
    event.verify().unwrap();
    assert_eq!(event.pubkey, state.gateway);
    state.submitted.send(event.clone()).unwrap();
    Json(json!({"event_id":event.id.to_hex(),"accepted":true,"message":""}))
}

async fn download(
    State(state): State<HttpRelay>,
    axum::extract::Path(file): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Result<Vec<u8>, StatusCode> {
    let auth = signed(&headers);
    assert_eq!(auth.pubkey, state.agent);
    assert_eq!(tag(&auth, "t")[0][1], "get");
    let hash = file.strip_suffix(".png").unwrap();
    state
        .blobs
        .lock()
        .unwrap()
        .get(hash)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)
}

#[tokio::test]
async fn acp_image_uses_signed_blossom_imeta_and_agent_authenticated_download() {
    for fail_upload in [false, true] {
        let ids = Ids::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
        let state = HttpRelay {
            url: url.clone(),
            gateway: ids.signer.public_key(),
            agent: ids.agent.public_key(),
            channel: ids.channel,
            fail_upload,
            submitted: submitted_tx,
            blobs: Arc::default(),
        };
        let router = Router::new()
            .route("/upload", put(upload))
            .route("/query", post(query))
            .route("/events", post(submit))
            .route("/media/{file}", get(download))
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let relay = Arc::new(super::super::ClientRelay {
            client: crate::client::BuzzClient::new(url.clone(), ids.signer.clone(), None, None)
                .unwrap(),
            scope: ids.scope(),
        });
        let resender = Arc::clone(&relay);
        let (relay_tx, relay_rx) = mpsc::channel(8);
        let (stdin, input) = tokio::io::duplex(1 << 16);
        let (output, stdout) = tokio::io::duplex(1 << 16);
        let bridge = tokio::spawn(run_bridge(
            ids.scope(),
            relay,
            relay_rx,
            BufReader::new(input),
            output,
            Timing::default(),
        ));
        let mut client = Client {
            stdin,
            stdout: BufReader::new(stdout).lines(),
        };
        relay_tx.send(RelayInbound::Live).await.unwrap();
        let (init, _) = client.request(1, "initialize", json!({})).await;
        assert_eq!(
            init["result"]["agentCapabilities"]["promptCapabilities"]["image"],
            true
        );
        let (new, _) = client.request(2, "session/new", json!({})).await;
        let session = new["result"]["sessionId"].as_str().unwrap();
        client.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"inspect this screenshot"},{"type":"image","mimeType":"image/png","data":PNG}]}})).await;
        if fail_upload {
            let (failed, _) = client.response(3).await;
            assert!(
                failed["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("image upload failed"),
                "{failed}"
            );
            assert!(
                submitted.try_recv().is_err(),
                "text must not post when upload fails"
            );
        } else {
            let message = recv(&mut submitted).await;
            let imeta = tag(&message, "imeta");
            assert_eq!(imeta.len(), 1);
            assert!(imeta[0].iter().any(|part| part == "m image/png"));
            assert_eq!(tag(&message, "h")[0][1], ids.channel.to_string());
            assert_eq!(tag(&message, "p")[0][1], ids.agent.public_key().to_hex());
            let media = imeta[0]
                .iter()
                .find_map(|part| part.strip_prefix("url "))
                .unwrap();
            assert!(message.content.contains(&format!("![image]({media})")));
            // `/recover` and `/fresh` resend the user's text with the uploaded
            // copy instead of new bytes: nothing is uploaded again, and the
            // image is linked once.
            let resent = super::super::bridge::BridgeRelay::prepare_content(
                resender.as_ref(),
                super::super::prompt::Prompt {
                    text: "inspect this screenshot".into(),
                    images: Vec::new(),
                    media: imeta.iter().map(|tag| tag.to_vec()).collect(),
                },
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                resent
                    .content
                    .matches(&format!("![image]({media})"))
                    .count(),
                1,
                "{}",
                resent.content
            );
            assert_eq!(tag(&resent, "imeta"), imeta);
            let agent = crate::client::BuzzClient::new(url, ids.agent.clone(), None, None).unwrap();
            assert_eq!(
                agent.download_media(media).await.unwrap().as_ref(),
                STANDARD.decode(PNG).unwrap()
            );
        }
        drop(client);
        bridge.await.unwrap().unwrap();
        server.abort();
    }
}

#[test]
fn image_limits_and_mime_are_checked_before_upload() {
    use super::super::prompt::{parse, MAX_IMAGES, MAX_IMAGE_BYTES};
    let image = json!({"type":"image","mimeType":"image/png","data":PNG});
    assert_eq!(
        parse(Some(&json!([image.clone()]))).unwrap().images.len(),
        1
    );
    assert!(parse(Some(&json!(vec![image; MAX_IMAGES + 1]))).is_err());
    let mut oversized = STANDARD.decode(PNG).unwrap();
    oversized.resize(MAX_IMAGE_BYTES + 1, 0);
    for block in [
        json!({"type":"image","mimeType":"image/svg+xml","data":PNG}),
        json!({"type":"image","mimeType":"image/jpeg","data":PNG}),
        json!({"type":"image","mimeType":"image/png","data":"!"}),
        json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(oversized)}),
    ] {
        assert!(parse(Some(&json!([block]))).is_err());
    }
}
