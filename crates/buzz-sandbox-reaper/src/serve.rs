//! `serve`: the session manager's relay loop (`docs/remote-agents.md`
//! §Relay transport). It authenticates to the relay as the manager key,
//! subscribes to kind 24210 events `p`-tagged to it, admits each one
//! ([`crate::manager`]), and publishes the encrypted replies.
//!
//! Logs carry only event ids, operations and codes. `buzz_ws_client` logs
//! whole frames at debug level, so `main` caps that target at info.

use crate::codex_login::server::{Options as CodexLoginOptions, Server as CodexLoginServer};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use buzz_backend_kubernetes::provider;
use buzz_backend_kubernetes::wire::Response;
use buzz_session_control::{build_reply, Op, Reply, KIND};
use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use clap::Args;
use futures_util::FutureExt;
use nostr::{Event, Keys, Tag, Timestamp};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::{join, select};
use tokio_util::sync::CancellationToken;

use crate::enroll::{self, Enrollment, SecretsManagerStore};
use crate::manager::{response_value, Admission, Job, Manager, ManagerConfig, Waiter};
use crate::shutdown;

/// Bound on connecting, authenticating and subscribing.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// First and largest pause between reconnection attempts.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long a read waits before the loop checks its other inputs again.
const READ_IDLE: Duration = Duration::from_secs(30);
/// Finished operations' replies waiting to be published.
const REPLY_QUEUE: usize = 256;
const SUBSCRIPTION: &str = "session-control";

#[derive(Args)]
pub struct ServeArgs {
    #[command(flatten)]
    codex: CodexLoginOptions,
    /// Path to the JSON config (ConfigMap `sandbox-manager-config`, key `config.json`).
    #[arg(
        long,
        env = "BUZZ_SANDBOX_MANAGER_CONFIG",
        default_value = "/etc/buzz-sandbox-manager/config.json"
    )]
    config: PathBuf,
    /// The relay's WebSocket URL.
    #[arg(long, env = "BUZZ_RELAY_URL")]
    relay_url: String,
    /// File holding the manager's private key (nsec or hex), mounted from a
    /// Secret. Without it, the key is read from `BUZZ_PRIVATE_KEY`.
    #[arg(long, env = "BUZZ_PRIVATE_KEY_FILE")]
    private_key_file: Option<PathBuf>,
    /// NIP-OA attestation of the manager key, when the relay admits it as an
    /// admin member's agent rather than as a member itself.
    #[arg(long, env = "BUZZ_AUTH_TAG", hide_env_values = true)]
    auth_tag: Option<String>,
    /// JSON object of Kubernetes `provider_config` fields set on every
    /// request, replacing the request's own: at least a digest-pinned
    /// `image`, plus `pod_options` and the other Pod settings. Required by
    /// `buzz-backend-relay` clients, which send none of them.
    #[arg(long, env = "BUZZ_SANDBOX_PROVIDER_CONFIG")]
    sandbox_provider_config: Option<String>,
    /// GitHub REST API base for the enrollment login check.
    #[arg(long, env = "BUZZ_GITHUB_API_URL", default_value = enroll::DEFAULT_GITHUB_API_URL)]
    github_api_url: String,
}

fn sandbox_defaults(text: Option<&str>) -> Result<serde_json::Map<String, serde_json::Value>> {
    let Some(text) = text else {
        return Ok(serde_json::Map::new());
    };
    let serde_json::Value::Object(defaults) = serde_json::from_str(text)
        .map_err(|_| anyhow::anyhow!("BUZZ_SANDBOX_PROVIDER_CONFIG is not valid JSON"))?
    else {
        anyhow::bail!("BUZZ_SANDBOX_PROVIDER_CONFIG must be a JSON object");
    };
    provider::check_managed_defaults(&defaults)
        .map_err(|e| anyhow::anyhow!("BUZZ_SANDBOX_PROVIDER_CONFIG: {e}"))?;
    Ok(defaults)
}

pub async fn serve(args: ServeArgs) -> Result<()> {
    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("read config {}", args.config.display()))?;
    let config = ManagerConfig::parse(&text)?;
    let defaults = sandbox_defaults(args.sandbox_provider_config.as_deref())?;
    let keys = load_keys(args.private_key_file.as_deref())?;
    let auth_tag = args
        .auth_tag
        .as_deref()
        .map(buzz_sdk::nip_oa::parse_auth_tag)
        .transpose()
        .map_err(|_| anyhow::anyhow!("BUZZ_AUTH_TAG is not a valid owner attestation"))?;
    let github = enroll::GitHub::new(&args.github_api_url)
        .map_err(|e| anyhow::anyhow!("BUZZ_GITHUB_API_URL: {e}"))?;
    let store = Arc::new(SecretsManagerStore::new().await);
    let broker = CodexLoginServer::prepare(args.codex, store.clone(), config.developers()).await?;
    let mut enrollment = Enrollment::new(github, store);
    if let Some(server) = &broker {
        enrollment = enrollment.with_broker(Arc::clone(&server.broker));
    }
    let client = kube::Client::try_default()
        .await
        .context("build in-cluster Kubernetes client")?;
    let developers = config.len();
    let manager =
        Arc::new(Manager::new(keys, config, client, enrollment).with_sandbox_defaults(defaults));
    tracing::info!(manager = %manager.keys().public_key(), developers,
        "buzz-sandbox-reaper serve started");
    if let Some(broker) = broker {
        let stop = CancellationToken::new();
        let relay = async {
            run(manager, &args.relay_url, auth_tag, stop.cancelled()).await;
            stop.cancel();
        };
        let signal = async {
            select! {
                () = shutdown() => stop.cancel(),
                () = stop.cancelled() => {},
            }
        };
        let (result, (), ()) = join!(broker.run(stop.clone()), relay, signal);
        result?;
    } else {
        run(manager, &args.relay_url, auth_tag, shutdown()).await;
    }
    Ok(())
}

/// Never echoes the key or the file's content.
fn load_keys(file: Option<&std::path::Path>) -> Result<Keys> {
    let secret = match file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("read private key file {}", path.display()))?,
        None => std::env::var("BUZZ_PRIVATE_KEY")
            .context("set BUZZ_PRIVATE_KEY_FILE or BUZZ_PRIVATE_KEY")?,
    };
    Keys::parse(secret.trim()).map_err(|_| anyhow::anyhow!("the manager private key is invalid"))
}

/// Serve until `shutdown`, reconnecting with capped exponential backoff.
/// Operations still running at shutdown are aborted: deploy and stop are
/// idempotent and journaled, so the client's retry resumes them.
pub async fn run(
    manager: Arc<Manager>,
    relay_url: &str,
    auth_tag: Option<Tag>,
    shutdown: impl Future<Output = ()>,
) {
    let (replies, mut queued) = mpsc::channel(REPLY_QUEUE);
    let mut jobs = JoinSet::new();
    tokio::pin!(shutdown);
    let mut backoff = MIN_BACKOFF;
    loop {
        let connected = select! {
            () = &mut shutdown => break,
            connected = tokio::time::timeout(
                CONNECT_TIMEOUT,
                connect(manager.keys(), relay_url, auth_tag.as_ref()),
            ) => connected.unwrap_or(Err("timeout")),
        };
        match connected {
            Ok(mut connection) => {
                backoff = MIN_BACKOFF;
                tracing::info!("subscribed to session control requests");
                let mut session = Session {
                    manager: &manager,
                    connection: &mut connection,
                    replies: &replies,
                    jobs: &mut jobs,
                };
                match session.serve(&mut queued, &mut shutdown).await {
                    None => break,
                    Some(error) => tracing::warn!(error, "relay connection lost"),
                }
            }
            Err(error) => tracing::warn!(error, "could not connect to the relay"),
        }
        select! {
            () = &mut shutdown => break,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    tracing::info!(running = jobs.len(), "shutting down");
    jobs.shutdown().await;
}

async fn connect(
    keys: &Keys,
    relay_url: &str,
    auth_tag: Option<&Tag>,
) -> Result<NostrWsConnection, &'static str> {
    let mut connection = NostrWsConnection::connect_authenticated(relay_url, keys, auth_tag)
        .await
        .map_err(|e| label(&e))?;
    let filter =
        serde_json::json!({"kinds": [KIND], "#p": [keys.public_key().to_hex()], "limit": 0});
    connection
        .send_raw(&serde_json::json!(["REQ", SUBSCRIPTION, filter]))
        .await
        .map_err(|e| label(&e))?;
    Ok(connection)
}

/// A stable label for a connection error. The error's own text can quote a
/// whole relay frame, so it is never logged.
fn label(error: &WsClientError) -> &'static str {
    match error {
        WsClientError::WebSocket(_) => "websocket",
        WsClientError::Json(_) => "json",
        WsClientError::EventBuilder(_) => "event_builder",
        WsClientError::Url(_) => "url",
        WsClientError::Timeout => "timeout",
        WsClientError::ConnectionClosed => "connection_closed",
        WsClientError::UnexpectedMessage(_) => "unexpected_message",
        WsClientError::AuthFailed(_) => "auth_failed",
        WsClientError::EventRejected(_) => "event_rejected",
        WsClientError::NoAuthChallenge => "no_auth_challenge",
    }
}

/// One connection's loop.
struct Session<'a> {
    manager: &'a Arc<Manager>,
    connection: &'a mut NostrWsConnection,
    replies: &'a mpsc::Sender<Event>,
    jobs: &'a mut JoinSet<()>,
}

impl Session<'_> {
    /// Serve until `shutdown` (`None`) or a connection failure (its label).
    async fn serve(
        &mut self,
        queued: &mut mpsc::Receiver<Event>,
        shutdown: &mut std::pin::Pin<&mut impl Future<Output = ()>>,
    ) -> Option<&'static str> {
        loop {
            select! {
                () = shutdown.as_mut() => return None,
                Some(reply) = queued.recv() => {
                    if let Err(error) = self.publish(reply).await {
                        return Some(error);
                    }
                }
                Some(_) = self.jobs.join_next() => {}
                message = self.connection.next_event(READ_IDLE) => match message {
                    Ok(RelayMessage::Event { event, .. }) => {
                        if let Some(reply) = self.admit(&event) {
                            if let Err(error) = self.publish(reply).await {
                                return Some(error);
                            }
                        }
                    }
                    Ok(RelayMessage::Closed { .. }) => return Some("subscription_closed"),
                    Ok(_) | Err(WsClientError::Timeout) => {}
                    // One malformed frame does not end the connection.
                    Err(WsClientError::Json(_) | WsClientError::UnexpectedMessage(_)) => {}
                    Err(error) => return Some(label(&error)),
                },
            }
        }
    }

    /// A reply the relay refuses is logged and dropped; the client's retry
    /// is answered from the manager's record.
    async fn publish(&mut self, reply: Event) -> Result<(), &'static str> {
        let event_id = reply.id;
        let ok = self
            .connection
            .send_event(reply)
            .await
            .map_err(|e| label(&e))?;
        if !ok.accepted {
            tracing::warn!(event_id = %event_id, "the relay refused a reply");
        }
        Ok(())
    }

    /// Admit one event, start its operation if authorized, and return the
    /// immediate reply.
    fn admit(&mut self, event: &Event) -> Option<Event> {
        let now = Timestamp::now();
        match self.manager.admit(event, now) {
            Admission::Ignore { event, error } => {
                tracing::info!(event_id = %event, error = error.label(), "ignored a request");
                None
            }
            Admission::Done {
                to,
                request_id,
                op,
                response,
            } => {
                tracing::info!(event_id = %to.event, op = op.map(Op::as_str),
                    code = response["code"].as_str(), "answered a request");
                reply(self.manager, to, &Reply::done(request_id, response))
            }
            Admission::Joined { to, request_id, op } => {
                tracing::info!(event_id = %to.event, op = op.as_str(), "joined a running request");
                reply(self.manager, to, &Reply::accepted(request_id))
            }
            Admission::Start {
                to,
                request_id,
                op,
                job,
            } => {
                tracing::info!(event_id = %to.event, op = op.as_str(), "accepted a request");
                self.jobs.spawn(execute(
                    Arc::clone(self.manager),
                    job,
                    (to, request_id.clone(), op),
                    self.replies.clone(),
                ));
                reply(self.manager, to, &Reply::accepted(request_id))
            }
        }
    }
}

/// Run one operation, record its response, and queue `done` for every
/// waiter. A panic is recorded as a failure, so the request never stays
/// running.
async fn execute(
    manager: Arc<Manager>,
    job: Box<Job>,
    (first, request_id, op): (Waiter, String, Op),
    replies: mpsc::Sender<Event>,
) {
    let Job { key, work } = *job;
    let response = AssertUnwindSafe(work.run(&manager))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            response_value(&Response::error(
                "the session manager failed while running the request",
            ))
        });
    tracing::info!(event_id = %first.event, op = op.as_str(), ok = response["ok"].as_bool(),
        code = response["code"].as_str(), "finished a request");
    for waiter in manager.complete(&key, response.clone(), Timestamp::now()) {
        let done = Reply::done(request_id.clone(), response.clone());
        if let Some(event) = reply(&manager, waiter, &done) {
            // Closed only at shutdown, when the reply could not be sent anyway.
            let _ = replies.send(event).await;
        }
    }
}

fn reply(manager: &Manager, to: Waiter, reply: &Reply) -> Option<Event> {
    build_reply(
        manager.keys(),
        &to.requester,
        &to.event,
        reply,
        Timestamp::now(),
    )
    .inspect_err(|error| {
        tracing::warn!(event_id = %to.event, error = error.label(), "could not build a reply");
    })
    .ok()
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "relay_provider_tests.rs"]
mod relay_provider_tests;
