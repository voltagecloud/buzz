//! Exercise the built CLI against an HTTP boundary, including byte-exact downloads.
use axum::{
    body::Bytes,
    extract::State,
    http::HeaderMap,
    routing::{get, post, put},
    Json, Router,
};
use nostr::{Event, Keys};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

const FIXTURE: &[u8] = include_bytes!("fixtures/bitgo-events-test.jsonl");

#[derive(Clone)]
struct Relay {
    url: String,
    hash: String,
    events: Arc<Mutex<Vec<Event>>>,
}

async fn upload(State(relay): State<Relay>, headers: HeaderMap, body: Bytes) -> Json<Value> {
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(headers["x-sha-256"], relay.hash);
    assert_eq!(body.as_ref(), FIXTURE);
    assert!(headers["authorization"]
        .to_str()
        .unwrap()
        .starts_with("Nostr "));
    Json(json!({
        "url": format!("{}/media/{}.bin", relay.url, relay.hash),
        "sha256": relay.hash, "size": body.len(),
        "type": "application/octet-stream", "uploaded": 0
    }))
}

async fn publish(State(relay): State<Relay>, Json(event): Json<Event>) -> Json<Value> {
    event.verify().unwrap();
    let id = event.id.to_hex();
    relay.events.lock().unwrap().push(event);
    Json(json!({"event_id": id, "accepted": true, "message": ""}))
}

async fn query(State(relay): State<Relay>, Json(filters): Json<Value>) -> Json<Value> {
    let filters = filters.as_array().unwrap();
    if filters
        .iter()
        .any(|f| f["kinds"].as_array().is_some_and(|k| k.contains(&json!(9))))
    {
        return Json(serde_json::to_value(relay.events.lock().unwrap().clone()).unwrap());
    }
    Json(json!([]))
}

async fn invoke(relay: &str, keys: &Keys, args: &[&str]) -> std::process::Output {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_buzz"))
            .kill_on_drop(true)
            .args(args)
            .env("BUZZ_RELAY_URL", relay)
            .env("BUZZ_PRIVATE_KEY", keys.secret_key().to_secret_hex())
            .env("BUZZ_OUTPUT_FORMAT", "json")
            .env_remove("BUZZ_AUTH_TAG")
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    output
}

async fn cli(relay: &str, keys: &Keys, args: &[&str]) -> std::process::Output {
    let output = invoke(relay, keys, args).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[tokio::test]
async fn jsonl_message_attachment_round_trip_and_standalone_upload() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = Relay {
        url: format!("http://{}", listener.local_addr().unwrap()),
        hash: hex::encode(Sha256::digest(FIXTURE)),
        events: Arc::default(),
    };
    let media_path = format!("/media/{}.bin", relay.hash);
    let app = Router::new()
        .route("/upload", put(upload))
        .route("/events", post(publish))
        .route("/query", post(query))
        .route(
            &media_path,
            get(|headers: HeaderMap| async move {
                assert!(headers["authorization"]
                    .to_str()
                    .unwrap()
                    .starts_with("Nostr "));
                FIXTURE
            }),
        )
        .with_state(relay.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let keys = Keys::generate();
    let channel = "b462f128-d11a-4ee1-951b-dcef9fccbbd6";
    for (filename, label) in [
        ("bitgo-events-test.jsonl", "bitgo-events-test.jsonl"),
        ("events[1].jsonl", "events\\[1\\].jsonl"),
    ] {
        let file = dir.path().join(filename);
        std::fs::write(&file, FIXTURE).unwrap();
        cli(
            &relay.url,
            &keys,
            &[
                "messages",
                "send",
                "--channel",
                channel,
                "--content",
                "JSONL fixture",
                "--file",
                file.to_str().unwrap(),
            ],
        )
        .await;
        let output = cli(
            &relay.url,
            &keys,
            &["messages", "get", "--channel", channel],
        )
        .await;
        let events: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        let event = events.last().unwrap();
        let url = format!("{}{}", relay.url, media_path);
        assert_eq!(event["content"], format!("JSONL fixture\n[{label}]({url})"));
        let tags = event["tags"].as_array().unwrap();
        let imeta = tags
            .iter()
            .find(|t| t[0] == "imeta")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(imeta.contains(&json!("m application/octet-stream")));
        assert!(imeta.contains(&json!(format!("url {url}"))));
        let download = dir.path().join("download.jsonl");
        cli(
            &relay.url,
            &keys,
            &["media", "get", &url, "--output", download.to_str().unwrap()],
        )
        .await;
        assert_eq!(std::fs::read(download).unwrap(), FIXTURE);
        let output = cli(
            &relay.url,
            &keys,
            &["upload", "file", "--file", file.to_str().unwrap()],
        )
        .await;
        let descriptor: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(descriptor["type"], "application/octet-stream");
    }
    let mut elf = [0u8; 64];
    elf[..4].copy_from_slice(b"\x7fELF");
    for (filename, bytes, mime) in [
        ("drawing.SVG", b"<svg></svg>".as_slice(), "image/svg+xml"),
        ("program.elf", elf.as_slice(), "application/x-executable"),
        (
            "program.exe",
            b"MZexecutable".as_slice(),
            "application/vnd.microsoft.portable-executable",
        ),
    ] {
        let file = dir.path().join(filename);
        std::fs::write(&file, bytes).unwrap();
        let output = invoke(
            &relay.url,
            &keys,
            &["upload", "file", "--file", file.to_str().unwrap()],
        )
        .await;
        assert!(!output.status.success(), "accepted {filename}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(mime));
    }
    server.abort();
}
