//! The actual command must warn on stderr without changing its JSON output.
use axum::{extract::State, routing::post, Json, Router};
use buzz_sdk::gateway::Delegation;
use nostr::{Event, Keys, Timestamp};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn gateway_list_reports_disabled_head_pressure_on_stderr() {
    let owner = Keys::generate();
    let heads = Arc::new(Mutex::new(Vec::<Event>::new()));
    let app =
        Router::new()
            .route(
                "/query",
                post(
                    |State(heads): State<Arc<Mutex<Vec<Event>>>>,
                     Json(filters): Json<Vec<Value>>| async move {
                        assert_eq!(filters[0]["consistency"], "strong");
                        assert_eq!(filters[0]["limit"], 32);
                        let mut events = heads.lock().unwrap().clone();
                        events
                            .sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
                        events.truncate(32);
                        Json(events)
                    },
                ),
            )
            .with_state(heads.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    for (count, warn) in [(27, false), (28, true), (36, true)] {
        *heads.lock().unwrap() = (0..count)
            .map(|i| {
                Delegation::revoke(
                    Keys::generate().public_key(),
                    Timestamp::now().as_secs() - i as u64,
                )
                .unwrap()
                .sign_with_keys(&owner)
                .unwrap()
            })
            .collect();
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_buzz"))
                .args(["gateway", "list"])
                .env("HOME", home.path())
                .env("BUZZ_RELAY_URL", &relay)
                .env("BUZZ_PRIVATE_KEY", owner.secret_key().to_secret_hex())
                .env("BUZZ_OUTPUT_FORMAT", "json")
                .env_remove("BUZZ_AUTH_TAG")
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let events: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(events.len(), count.min(32));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            stderr.contains("gateway authority checks only the newest 32"),
            warn,
            "{stderr}"
        );
        if warn {
            assert!(stderr.contains("disabled heads") && stderr.contains("Renew"));
        }
    }
    server.abort();
}
