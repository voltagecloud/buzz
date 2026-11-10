//! The Sandbox manager boundary: kind 24210 session control
//! (`buzz-session-control`). Requests are NIP-44 encrypted to the manager
//! and replies are opened only when the manager signed them for this
//! request, exactly as the Buzz app's enroll form and its relay provider
//! (`buzz-backend-relay`) do. The transport only moves events; building
//! and opening them stays here, so tests exercise the real wire format.

use std::time::Duration;

use buzz_session_control::{build_request, open_reply, Op, Request, Status, KIND};
use nostr::{Event, PublicKey, Timestamp};
use serde_json::{json, Value};

use super::relay::Signer;
use crate::error::CliError;

/// How long the manager has to answer `enroll` and `profile`, as in the app.
pub const OWNER_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a first start may take: the relay provider's 600-second default
/// plus its 30 seconds to be accepted.
pub const DEPLOY_TIMEOUT: Duration = Duration::from_secs(630);

/// Moves session control events between a signer and the manager.
pub trait ControlTransport {
    /// Subscribes to kind 24210 events addressed to `signer`, publishes
    /// `request`, and passes each event received to `open` until it returns
    /// the terminal response or `timeout` passes.
    async fn exchange<F>(
        &self,
        signer: &Signer,
        request: Event,
        timeout: Duration,
        open: F,
    ) -> Result<Value, CliError>
    where
        F: FnMut(&Event) -> Option<Value>;
}

/// Sends one `op` request with `payload` to `manager`, signed by `signer`,
/// and returns the manager's terminal response. Only a `done` reply that the
/// manager signed for this request event is read; anything else is skipped.
pub async fn ask<T: ControlTransport>(
    transport: &T,
    manager: &PublicKey,
    signer: &Signer,
    op: Op,
    payload: Value,
    timeout: Duration,
) -> Result<Value, CliError> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let envelope = Request::new(op, request_id.clone(), payload);
    let event = build_request(
        &signer.keys,
        manager,
        signer.auth_tag.as_deref(),
        &envelope,
        Timestamp::now(),
    )
    .map_err(|error| {
        CliError::Other(format!(
            "could not build the {} request ({})",
            op.as_str(),
            error.label()
        ))
    })?;
    drop(envelope);
    let request_event = event.id;
    transport
        .exchange(signer, event, timeout, |reply| {
            let reply = open_reply(reply, &signer.keys, manager, &request_event).ok()?;
            (reply.request_id == request_id && reply.status == Status::Done)
                .then(|| reply.response.unwrap_or(Value::Null))
        })
        .await
}

/// The relay's WebSocket, authenticated as the signer (NIP-42, with the
/// owner attestation for an agent key).
pub struct WsTransport {
    ws_url: String,
}

impl WsTransport {
    /// The transport for the `wss://` community `relay`.
    pub fn new(relay: &str) -> Self {
        Self {
            ws_url: relay.trim_end_matches('/').to_owned(),
        }
    }
}

const SUBSCRIPTION: &str = "onboard-session-control";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

impl ControlTransport for WsTransport {
    async fn exchange<F>(
        &self,
        signer: &Signer,
        request: Event,
        timeout: Duration,
        mut open: F,
    ) -> Result<Value, CliError>
    where
        F: FnMut(&Event) -> Option<Value>,
    {
        use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
        let unavailable =
            || CliError::Other("could not connect, authenticate or subscribe to the relay".into());
        let tag = match &signer.auth_tag {
            Some(json) => Some(
                buzz_sdk::nip_oa::parse_auth_tag(json)
                    .map_err(|_| CliError::Auth("the agent attestation is malformed".into()))?,
            ),
            None => None,
        };
        let connect = async {
            let mut connection =
                NostrWsConnection::connect_authenticated(&self.ws_url, &signer.keys, tag.as_ref())
                    .await?;
            let filter = json!({
                "kinds": [KIND], "#p": [signer.keys.public_key().to_hex()], "limit": 0,
            });
            connection
                .send_raw(&json!(["REQ", SUBSCRIPTION, filter]))
                .await?;
            // Replies are ephemeral: listen before sending.
            loop {
                match connection.next_event(CONNECT_TIMEOUT).await? {
                    RelayMessage::Eose { subscription_id } if subscription_id == SUBSCRIPTION => {
                        return Ok::<_, WsClientError>(connection);
                    }
                    RelayMessage::Closed {
                        subscription_id, ..
                    } if subscription_id == SUBSCRIPTION => {
                        return Err(WsClientError::ConnectionClosed);
                    }
                    _ => {}
                }
            }
        };
        let mut connection = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?;
        let lost = || {
            CliError::Other("lost the relay connection before the Sandbox manager answered".into())
        };
        let ok = connection.send_event(request).await.map_err(|_| lost())?;
        if !ok.accepted {
            return Err(CliError::Other(
                "the relay refused the session request".into(),
            ));
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let result = loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break Err(CliError::Other(format!(
                    "the Sandbox manager did not answer within {} seconds",
                    timeout.as_secs()
                )));
            }
            match connection.next_event(deadline - now).await {
                Ok(RelayMessage::Event {
                    subscription_id,
                    event,
                }) if subscription_id == SUBSCRIPTION => {
                    if let Some(response) = open(&event) {
                        break Ok(response);
                    }
                }
                Ok(RelayMessage::Closed {
                    subscription_id, ..
                }) if subscription_id == SUBSCRIPTION => break Err(lost()),
                Ok(_) | Err(WsClientError::Timeout) => {}
                Err(WsClientError::Json(_) | WsClientError::UnexpectedMessage(_)) => {}
                Err(_) => break Err(lost()),
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), connection.disconnect()).await;
        result
    }
}

/// The manager key the Buzz app's Remote workspace agents ask
/// (`MANAGER_PUBKEY` in GPUI's `remote_workspace/config.rs`).
pub const MANAGER_PUBKEY: &str = "1881c0c6186c81c00e607f9dcd9b6c655c49cd9a0227054e764a78e6829d31fd";

/// The manager's refusal of a first-start `deploy`, as `code: message`,
/// with what a `paseo_store_failed` refusal leaves behind.
pub fn refusal_text(response: &Value) -> String {
    let code = response
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("refused");
    let text = match response.get("error").and_then(Value::as_str) {
        Some(message) => format!("{code}: {message}"),
        None => code.to_owned(),
    };
    if code == "paseo_store_failed" {
        // The manager refuses after the Sandbox started and the launch
        // registration was saved; repeating the deploy retries the update.
        format!("{text} (it started, but your Paseo daemon's agent list was not updated)")
    } else {
        text
    }
}
