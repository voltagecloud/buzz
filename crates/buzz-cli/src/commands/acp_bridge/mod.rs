//! `buzz acp-bridge`: an ACP agent over stdio that drives one Buzz remote
//! agent through the relay.
//!
//! An ACP client (for example Paseo) starts the bridge as an agent process.
//! Each ACP session is one Buzz thread in `--channel`: the first prompt posts
//! a kind 9 thread root that mentions `--agent` (the mention starts the
//! agent's sandbox), and later prompts post kind 9 replies in that thread.
//! The agent's encrypted observer telemetry (kind 24200) streams back as
//! `session/update` notifications, and `turn_completed` ends the prompt. The
//! Buzz thread stays the durable record.

mod bridge;
mod frames;
mod names;
mod projects;
mod prompt;
mod views;
mod wire;
mod workspace_paths;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "relay_stream_tests.rs"]
mod relay_stream_tests;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Event, Keys, PublicKey, Tag};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::Instant;
use uuid::Uuid;

use crate::client::{normalize_write_response, to_ws_url, BuzzClient};
use crate::commands::messages::{prepare_message, SendMessageParams};
use crate::error::CliError;
use crate::validate::parse_uuid;
use bridge::{BridgeRelay, RelayInbound, Timing};
use frames::Scope;

/// Bound on relay events waiting for the bridge loop.
const RELAY_QUEUE: usize = 1024;
/// Consecutive failed connections after which the bridge reports the relay
/// as unreachable to the ACP client. It keeps retrying at [`MAX_BACKOFF`].
const SURFACE_AFTER_FAILURES: u32 = 5;
/// Reconnect wait after the first failure; it doubles per failure.
const BASE_BACKOFF: Duration = Duration::from_secs(1);
/// Longest wait between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A live connection resets the backoff only when it stays live this long or
/// delivers a live event. A relay that accepts each connection and then
/// drops it is backed off like one that refuses it.
const HEALTHY_AFTER: Duration = Duration::from_secs(30);
/// Replay window for channel messages before the moment a live connection
/// dropped; duplicates are dropped by event id.
const RECONNECT_OVERLAP_SECS: u64 = 120;
/// The relay pings every 30 s. A connection silent for this long, pings
/// included, is treated as dead and replaced.
const IDLE_READ: Duration = Duration::from_secs(90);
/// How long one connection may take to become usable.
#[derive(Clone, Copy)]
struct Deadlines {
    /// TCP, TLS, the WebSocket upgrade, and NIP-42 authentication.
    connect: Duration,
    /// Both subscriptions catching up. Relay pings restart the idle read
    /// timeout, so this deadline does not depend on relay traffic.
    catch_up: Duration,
}

const DEADLINES: Deadlines = Deadlines {
    connect: Duration::from_secs(30),
    catch_up: Duration::from_secs(60),
};
const TELEMETRY_SUB: &str = "acp-bridge-telemetry";
const THREAD_SUB: &str = "acp-bridge-thread";

/// Refuse any relay URL that is not `wss://`. The bridge keeps one
/// long-lived authenticated WebSocket, and plain `ws://` is never used.
pub(crate) fn require_wss(raw: &str) -> Result<(), CliError> {
    let url = url::Url::parse(raw.trim()).map_err(|e| {
        CliError::Usage(format!(
            "acp-bridge requires BUZZ_RELAY_URL to be a wss:// URL: {e}"
        ))
    })?;
    if url.scheme() != "wss" || url.host_str().is_none_or(str::is_empty) {
        return Err(CliError::Usage(format!(
            "acp-bridge requires BUZZ_RELAY_URL to be a wss:// URL (got {}://); ws:// and http(s):// are refused",
            url.scheme()
        )));
    }
    Ok(())
}

/// Run `buzz acp-bridge`. `client` must point at a relay already checked by
/// [`require_wss`].
pub(crate) async fn run(
    client: BuzzClient,
    agent: &str,
    channel: &str,
    owner: Option<&str>,
) -> Result<(), CliError> {
    let agent = PublicKey::parse(agent.trim())
        .map_err(|e| CliError::Usage(format!("invalid --agent pubkey (hex or npub): {e}")))?;
    let channel = parse_uuid(channel)?;
    let scope = Scope {
        signer: client.keys().clone(),
        owner: owner
            .map(PublicKey::parse)
            .transpose()
            .map_err(|e| CliError::Usage(format!("invalid --owner pubkey: {e}")))?
            .unwrap_or_else(|| client.keys().public_key()),
        agent,
        channel,
        home: dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/")),
        paseo_code: std::path::PathBuf::from("paseo-code"),
    };

    let (relay_tx, relay_rx) = mpsc::channel(RELAY_QUEUE);
    let subscription = tokio::spawn(run_subscription(
        to_ws_url(client.relay_url()),
        scope.clone(),
        client.auth_tag().cloned(),
        relay_tx,
    ));
    let relay = Arc::new(ClientRelay {
        client,
        scope: scope.clone(),
    });
    let result = bridge::run_bridge(
        scope,
        relay,
        relay_rx,
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        Timing::default(),
    )
    .await;
    subscription.abort();
    result
}

/// [`BridgeRelay`] over the buzz-cli relay client.
struct ClientRelay {
    client: BuzzClient,
    scope: Scope,
}

impl BridgeRelay for ClientRelay {
    async fn query(&self, filter: serde_json::Value) -> Result<Vec<Event>, CliError> {
        let raw = match filter.as_array() {
            Some(filters) => self.client.query_multi(filters).await?,
            None => self.client.query(&filter).await?,
        };
        serde_json::from_str(&raw)
            .map_err(|e| CliError::Other(format!("invalid relay history response: {e}")))
    }

    async fn prepare_prompt(
        &self,
        content: String,
        thread_root: Option<String>,
    ) -> Result<Event, CliError> {
        let params = SendMessageParams {
            paseo_transcript: None,
            channel_id: self.scope.channel.to_string(),
            content,
            kind: Some(9),
            reply_to: thread_root,
            broadcast: false,
            files: Vec::new(),
            mentions: vec![self.scope.agent.to_hex()],
        };
        prepare_message(&self.client, &params).await
    }

    async fn prepare_content(
        &self,
        mut prompt: prompt::Prompt,
        thread_root: Option<String>,
    ) -> Result<Event, CliError> {
        // A resent prompt reuses earlier uploads; keep each one linked in
        // the text, as on the first send.
        for media in &prompt.media {
            let url = media.iter().find_map(|field| field.strip_prefix("url "));
            if let Some(url) = url.filter(|url| !prompt.text.contains(url)) {
                prompt.text.push_str(&format!("\n![image]({url})"));
            }
        }
        for image in prompt.images {
            let hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&image.bytes));
            let size = image.bytes.len() as u64;
            let descriptor = self
                .client
                .upload_bytes(image.bytes)
                .await
                .map_err(|error| {
                    CliError::Other(format!("image upload failed; prompt was not sent: {error}"))
                })?;
            if descriptor.sha256 != hash
                || descriptor.size != size
                || descriptor.mime_type != image.mime
            {
                return Err(CliError::Other(
                    "image upload returned inconsistent metadata; prompt was not sent".into(),
                ));
            }
            let uploaded = url::Url::parse(&descriptor.url)
                .map_err(|_| CliError::Other("image upload returned an invalid URL".into()))?;
            let relay = url::Url::parse(self.client.relay_url())
                .map_err(|_| CliError::Other("invalid relay URL".into()))?;
            if uploaded.origin() != relay.origin()
                || uploaded.fragment().is_some()
                || !uploaded.username().is_empty()
                || uploaded.password().is_some()
            {
                return Err(CliError::Other(
                    "image upload returned a foreign URL; prompt was not sent".into(),
                ));
            }
            prompt
                .text
                .push_str(&format!("\n![image]({})", descriptor.url));
            prompt
                .media
                .push(crate::client::build_imeta_tag(&descriptor));
        }
        let event = self.prepare_prompt(prompt.text, thread_root).await?;
        let mut tags = event.tags.to_vec();
        for media in prompt.media {
            tags.push(Tag::parse(media).map_err(|error| CliError::Other(error.to_string()))?);
        }
        nostr::EventBuilder::new(event.kind, event.content)
            .tags(tags)
            .custom_created_at(event.created_at)
            .sign_with_keys(self.client.keys())
            .map_err(|error| CliError::Other(error.to_string()))
    }

    async fn submit(&self, event: Event) -> Result<(), CliError> {
        let raw = self.client.submit_event(event).await?;
        let response: serde_json::Value =
            serde_json::from_str(&normalize_write_response(&raw)).unwrap_or_default();
        if response
            .get("accepted")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            Ok(())
        } else {
            Err(CliError::Relay {
                status: 400,
                body: raw,
            })
        }
    }

    async fn publish_ephemeral(&self, event: Event) -> Result<(), CliError> {
        self.client.publish_ephemeral_event(event).await.map(drop)
    }
}

fn now_secs() -> u64 {
    nostr::Timestamp::now().as_secs()
}

/// Keep the relay subscriptions alive, reconnecting with backoff.
async fn run_subscription(
    url: String,
    scope: Scope,
    auth_tag: Option<Tag>,
    tx: mpsc::Sender<RelayInbound>,
) {
    supervise(tx, rand::random::<f64>, move |since, tx| {
        let (url, scope, auth_tag) = (url.clone(), scope.clone(), auth_tag.clone());
        async move { stream_once(&url, &scope, auth_tag.as_ref(), since, &tx, DEADLINES).await }
    })
    .await;
}

/// Wait before reconnect attempt `failures` (1-based): exponential from
/// [`BASE_BACKOFF`], capped at [`MAX_BACKOFF`], scaled by `jitter` in
/// `[0, 1)` to between half and all of that.
pub(crate) fn backoff_delay(failures: u32, jitter: f64) -> Duration {
    let exponent = failures.saturating_sub(1).min(16);
    let full = BASE_BACKOFF.saturating_mul(1 << exponent).min(MAX_BACKOFF);
    full.mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0))
}

/// Run `connect` until the bridge stops listening. Each failure sends
/// [`RelayInbound::Down`]; from [`SURFACE_AFTER_FAILURES`] consecutive
/// failures on, each also sends [`RelayInbound::Unreachable`]. Retries never
/// stop. `jitter` returns values in `[0, 1)`.
pub(crate) async fn supervise<C, F>(
    tx: mpsc::Sender<RelayInbound>,
    mut jitter: impl FnMut() -> f64,
    mut connect: C,
) where
    C: FnMut(u64, mpsc::Sender<RelayInbound>) -> F,
    F: Future<Output = Stream>,
{
    let start = now_secs().saturating_sub(5);
    let mut live_until = start;
    let mut failures = 0u32;
    loop {
        let since = live_until.saturating_sub(RECONNECT_OVERLAP_SECS).max(start);
        let failed = match connect(since, tx.clone()).await {
            Stream::ReceiverGone => return,
            Stream::Failed(failed) => failed,
        };
        if let Some(live_at) = failed.live_at {
            // Everything up to the drop was delivered; replay only the
            // overlap before it.
            live_until = now_secs();
            if failed.live_event || live_at.elapsed() >= HEALTHY_AFTER {
                failures = 0;
            }
        }
        if tx.send(RelayInbound::Down).await.is_err() {
            return;
        }
        failures = failures.saturating_add(1);
        let delay = backoff_delay(failures, jitter());
        if failures >= SURFACE_AFTER_FAILURES {
            let message = format!(
                "acp-bridge: cannot reach the Buzz relay ({failures} failed connections in a row: {}); still retrying",
                failed.error
            );
            if tx.send(RelayInbound::Unreachable(message)).await.is_err() {
                return;
            }
        }
        eprintln!(
            "acp-bridge: relay connection lost ({}); reconnecting in {}s",
            failed.error,
            delay.as_secs()
        );
        tokio::time::sleep(delay).await;
    }
}

/// How one connection ended.
pub(crate) enum Stream {
    /// The bridge stopped listening.
    ReceiverGone,
    Failed(Failure),
}

pub(crate) struct Failure {
    pub error: String,
    /// When the subscriptions caught up, if they did.
    pub live_at: Option<Instant>,
    /// Whether a subscribed event arrived after the subscriptions caught up.
    pub live_event: bool,
}

async fn stream_once(
    url: &str,
    scope: &Scope,
    auth_tag: Option<&Tag>,
    since: u64,
    tx: &mpsc::Sender<RelayInbound>,
    deadlines: Deadlines,
) -> Stream {
    let mut live_at = None;
    let mut live_event = false;
    let failed = |error: String, live_at: Option<Instant>, live_event: bool| {
        Stream::Failed(Failure {
            error,
            live_at,
            live_event,
        })
    };
    let connect = NostrWsConnection::connect_authenticated(url, &scope.signer, auth_tag);
    let mut conn = match tokio::time::timeout(deadlines.connect, connect).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => return failed(e.to_string(), None, false),
        Err(_) => {
            return failed(
                format!(
                    "relay connection did not complete within {}s",
                    deadlines.connect.as_secs()
                ),
                None,
                false,
            )
        }
    };
    let requests = [
        json!([
            "REQ",
            TELEMETRY_SUB,
            telemetry_filter(&scope.signer, &scope.agent)
        ]),
        json!(["REQ", THREAD_SUB, thread_filter(scope.channel, since)]),
    ];
    for request in &requests {
        if let Err(e) = conn.send_raw(request).await {
            return failed(e.to_string(), None, false);
        }
    }

    let mut pending_eose = vec![TELEMETRY_SUB, THREAD_SUB];
    let catch_up_by = Instant::now() + deadlines.catch_up;
    loop {
        let next = conn.next_event(IDLE_READ);
        let next = if pending_eose.is_empty() {
            next.await
        } else {
            match tokio::time::timeout_at(catch_up_by, next).await {
                Ok(next) => next,
                Err(_) => {
                    return failed(
                        format!(
                            "relay subscriptions did not catch up within {}s",
                            deadlines.catch_up.as_secs()
                        ),
                        live_at,
                        live_event,
                    )
                }
            }
        };
        let message = match next {
            Ok(message) => message,
            Err(WsClientError::Timeout) => {
                return failed(
                    format!("no relay traffic for {}s", IDLE_READ.as_secs()),
                    live_at,
                    live_event,
                )
            }
            Err(e) => return failed(e.to_string(), live_at, live_event),
        };
        match message {
            RelayMessage::Event { event, .. } => {
                // Relay frames deserialize without a signature check, and the
                // bridge attributes agent text and session-ended notices by
                // signer, so an unverified event is dropped here.
                if event.verify().is_err() {
                    eprintln!("acp-bridge: dropped a relay event with an invalid signature");
                    continue;
                }
                live_event |= live_at.is_some();
                if tx.send(RelayInbound::Event(event)).await.is_err() {
                    return Stream::ReceiverGone;
                }
            }
            RelayMessage::Eose { subscription_id } => {
                let before = pending_eose.len();
                pending_eose.retain(|sub| *sub != subscription_id);
                if before > 0 && pending_eose.is_empty() {
                    live_at = Some(Instant::now());
                    if tx.send(RelayInbound::Live).await.is_err() {
                        return Stream::ReceiverGone;
                    }
                }
            }
            RelayMessage::Closed {
                subscription_id,
                message,
            } if subscription_id == TELEMETRY_SUB || subscription_id == THREAD_SUB => {
                return failed(
                    format!("relay closed subscription {subscription_id}: {message}"),
                    live_at,
                    live_event,
                );
            }
            RelayMessage::Notice { message } => eprintln!("acp-bridge: relay notice: {message}"),
            _ => {}
        }
    }
}

/// The driven agent's telemetry frames addressed to the signer. Frames are
/// ephemeral, so only live ones matter.
fn telemetry_filter(signer: &Keys, agent: &PublicKey) -> serde_json::Value {
    json!({
        "kinds": [buzz_core::kind::KIND_AGENT_OBSERVER_FRAME],
        "authors": [agent.to_hex()],
        "#p": [signer.public_key().to_hex()],
        "since": now_secs().saturating_sub(10),
    })
}

/// Channel messages; the bridge keeps the ones in its sessions' threads.
fn thread_filter(channel: Uuid, since: u64) -> serde_json::Value {
    json!({
        "kinds": [
            buzz_core::kind::KIND_STREAM_MESSAGE,
            buzz_core::kind::KIND_STREAM_MESSAGE_V2,
        ],
        "#h": [channel.to_string()],
        "since": since,
    })
}
