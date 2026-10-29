//! The private TLS refresh endpoint used by the pinned Codex client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header::CACHE_CONTROL, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_server::tls_rustls::RustlsConfig;
use axum_server::{bind_rustls, Handle};
use chrono::Utc;
use clap::Args;
use serde::Deserialize;
use serde_json::json;
use tokio::fs::read_to_string;
use tokio::sync::Semaphore;
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};
use tokio::{join, select};
use tokio_util::sync::CancellationToken;
use tower_http::timeout::TimeoutLayer;
use tracing::warn;
use zeroize::Zeroize;

use super::journal::DynamoJournal;
use super::oauth::CLIENT_ID;
use super::{Broker, Error, OAuth};
use crate::enroll::ProfileStore;
use crate::manager::Developer;

/// No listener or certificate access occurs unless `public_url` is set.
#[derive(Args)]
pub struct Options {
    #[arg(long, env = "BUZZ_CODEX_REFRESH_URL")]
    public_url: Option<String>,
    #[arg(long, env = "BUZZ_CODEX_JOURNAL_TABLE")]
    journal_table: Option<String>,
    #[arg(
        long,
        env = "BUZZ_CODEX_REFRESH_LISTEN",
        default_value = "0.0.0.0:8443"
    )]
    listen: SocketAddr,
    #[arg(
        long,
        env = "BUZZ_CODEX_CA_FILE",
        default_value = "/etc/buzz-codex-ca/ca.crt"
    )]
    ca_file: PathBuf,
    #[arg(
        long,
        env = "BUZZ_CODEX_TLS_CERT",
        default_value = "/etc/buzz-codex-tls/tls.crt"
    )]
    certificate: PathBuf,
    #[arg(
        long,
        env = "BUZZ_CODEX_TLS_KEY",
        default_value = "/etc/buzz-codex-tls/tls.key"
    )]
    private_key: PathBuf,
}

/// Concurrent refreshes across all developers; a bootstrap burst queues
/// here (within the 90 s request timeout) rather than failing a Sandbox.
const PERMITS: usize = 16;
const PERMIT_WAIT: Duration = Duration::from_secs(30);

/// The optional broker listener and its manager-owned login service.
pub struct Server {
    options: Options,
    tls: RustlsConfig,
    pub broker: Arc<Broker>,
}

impl Server {
    /// Load mounted certificates before accepting enrollment or refreshes.
    pub async fn prepare(
        options: Options,
        store: Arc<dyn ProfileStore>,
        developers: impl IntoIterator<Item = Developer>,
    ) -> Result<Option<Self>> {
        let Some(url) = options.public_url.clone() else {
            return Ok(None);
        };
        let certificate = read_to_string(&options.ca_file)
            .await
            .map_err(|_| anyhow!("could not read the ChatGPT broker CA certificate"))?;
        let broker = Arc::new(Broker::new(
            store,
            Arc::new(
                DynamoJournal::new(
                    options
                        .journal_table
                        .clone()
                        .ok_or_else(|| anyhow!("ChatGPT broker journal table is required"))?,
                )
                .await?,
            ),
            OAuth::new()?,
            developers,
            url,
            certificate,
        )?);
        let tls = RustlsConfig::from_pem_file(&options.certificate, &options.private_key)
            .await
            .map_err(|_| anyhow!("could not load the ChatGPT broker TLS certificate and key"))?;
        Ok(Some(Self {
            options,
            tls,
            broker,
        }))
    }

    /// Serve with bounded shutdown and reload renewed leaf certificates.
    pub async fn run(self, stop: CancellationToken) -> Result<()> {
        let handle = Handle::new();
        let server = bind_rustls(self.options.listen, self.tls.clone())
            .handle(handle.clone())
            .serve(router(self.broker).into_make_service());
        let serving = async {
            let result = server.await;
            stop.cancel();
            result.map_err(|_| anyhow!("ChatGPT broker listener failed"))
        };
        let stopping = async {
            stop.cancelled().await;
            handle.graceful_shutdown(Some(Duration::from_secs(20)));
        };
        let reloading = async {
            let mut interval = interval_at(
                Instant::now() + Duration::from_secs(60),
                Duration::from_secs(60),
            );
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                select! {
                    () = stop.cancelled() => break,
                    _ = interval.tick() => {
                        if self.tls.reload_from_pem_file(&self.options.certificate, &self.options.private_key).await.is_err() {
                            warn!("could not reload the ChatGPT broker TLS certificate");
                        }
                    }
                }
            }
        };
        let (result, (), ()) = join!(serving, stopping, reloading);
        result
    }
}

#[derive(Clone)]
struct Endpoint {
    broker: Arc<Broker>,
    permits: Arc<Semaphore>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Refresh {
    client_id: String,
    grant_type: String,
    refresh_token: String,
}

impl Drop for Refresh {
    fn drop(&mut self) {
        self.refresh_token.zeroize();
    }
}

pub(super) fn router(broker: Arc<Broker>) -> Router {
    router_with(broker, Arc::new(Semaphore::new(PERMITS)))
}

pub(super) fn router_with(broker: Arc<Broker>, permits: Arc<Semaphore>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/oauth/token", post(refresh))
        .layer(DefaultBodyLimit::max(1024))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(90),
        ))
        .with_state(Endpoint { broker, permits })
}

async fn refresh(
    State(endpoint): State<Endpoint>,
    payload: Result<Json<Refresh>, JsonRejection>,
) -> Response {
    let request = match payload {
        Ok(Json(request)) => request,
        Err(rejected) => {
            let status = if rejected.status() == StatusCode::PAYLOAD_TOO_LARGE {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return invalid_request(status);
        }
    };
    if request.client_id != CLIENT_ID || request.grant_type != "refresh_token" {
        return invalid_request(StatusCode::BAD_REQUEST);
    }
    let Ok(Ok(_permit)) = timeout(PERMIT_WAIT, endpoint.permits.acquire()).await else {
        return failure(Error::Busy);
    };
    match endpoint
        .broker
        .refresh(&request.refresh_token, Utc::now().timestamp())
        .await
    {
        Ok(response) => (
            StatusCode::OK,
            [(CACHE_CONTROL, "no-store")],
            Json(response),
        )
            .into_response(),
        Err(error) => failure(error),
    }
}

fn invalid_request(status: StatusCode) -> Response {
    (
        status,
        [(CACHE_CONTROL, "no-store")],
        Json(json!({"error": "invalid_request"})),
    )
        .into_response()
}

fn failure(error: Error) -> Response {
    let (status, code) = match error {
        Error::Unauthorized
        | Error::NotConfigured
        | Error::Interrupted
        | Error::Rejected
        | Error::InvalidLogin => (StatusCode::UNAUTHORIZED, "invalid_grant"),
        Error::Storage | Error::Upstream | Error::Busy => {
            (StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable")
        }
    };
    (
        status,
        [(CACHE_CONTROL, "no-store")],
        Json(json!({"error": code, "error_description": error.to_string()})),
    )
        .into_response()
}
