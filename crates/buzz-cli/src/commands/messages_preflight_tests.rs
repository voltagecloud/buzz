//! Exercise the production send preflight against actual HTTP responses.
use std::sync::Arc;

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use nostr::Keys;
use serde_json::{json, Value};

use super::{prepare_message, SendMessageParams};
use crate::{client::BuzzClient, error::CliError};

type Response = Arc<dyn Fn(Value) -> (StatusCode, String) + Send + Sync>;

struct Fixture {
    client: BuzzClient,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(
    response: impl Fn(Value) -> (StatusCode, String) + Send + Sync + 'static,
) -> Fixture {
    let app = Router::new()
        .route(
            "/query",
            post(
                |State(response): State<Response>, Json(filter): Json<Value>| async move {
                    response(filter)
                },
            ),
        )
        .with_state(Arc::new(response) as Response);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Fixture {
        client: BuzzClient::new(url, Keys::generate(), None, None).unwrap(),
        task,
    }
}

fn params(mention: String) -> SendMessageParams {
    SendMessageParams {
        paseo_transcript: None,
        channel_id: "6f1c1b2e-3d4a-4b5c-8d9e-0f1a2b3c4d5e".into(),
        content: "hello".into(),
        kind: None,
        reply_to: None,
        broadcast: false,
        files: vec![],
        mentions: vec![mention],
    }
}

#[tokio::test]
async fn exhausted_rate_limit_preserves_status_retry_hint_and_retryability() {
    let f = fixture(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error":"rate-limited: quota exceeded; retry in 0s"}).to_string(),
        )
    })
    .await;
    let error = prepare_message(&f.client, &params(Keys::generate().public_key().to_hex()))
        .await
        .unwrap_err();
    assert!(matches!(error, CliError::Relay { status: 429, .. }));
    assert!(error.to_string().contains("rate-limited"));
    assert!(crate::error::is_retryable_error(&error));
}

#[tokio::test]
async fn malformed_or_missing_membership_never_builds_a_message() {
    for body in ["[]", "{\"events\":[]}", "not-json"] {
        let f = fixture(move |_| (StatusCode::OK, body.into())).await;
        let error = prepare_message(&f.client, &params(Keys::generate().public_key().to_hex()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("mention preflight"));
        assert!(!crate::error::is_retryable_error(&error));
    }
}

#[tokio::test]
async fn membership_success_still_rejects_a_nonmember_mention() {
    let member = Keys::generate().public_key().to_hex();
    let f = fixture(move |_| (StatusCode::OK, json!([{"tags":[["p",member]]}]).to_string())).await;
    let error = prepare_message(&f.client, &params(Keys::generate().public_key().to_hex()))
        .await
        .unwrap_err();
    assert!(matches!(error, CliError::Usage(_)));
    assert!(error.to_string().contains("not channel members"));
}

#[tokio::test]
async fn member_profile_errors_preserve_the_relay_failure() {
    let member = Keys::generate().public_key().to_hex();
    let mut p = params(member.clone());
    p.content = "hello @Alice".into();
    let f = fixture(move |filter| {
        if filter[0]["kinds"] == json!([39002]) {
            (StatusCode::OK, json!([{"tags":[["p",member]]}]).to_string())
        } else {
            (StatusCode::FORBIDDEN, "profile access denied".into())
        }
    })
    .await;
    let error = prepare_message(&f.client, &p).await.unwrap_err();
    assert!(error.to_string().contains("profile access denied"));
    assert!(!crate::error::is_retryable_error(&error));
}
