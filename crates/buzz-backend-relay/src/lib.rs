//! The relay provider (`docs/remote-agents.md` §Relay transport): the
//! provider protocol on stdin and stdout, with each `deploy` and `stop`
//! forwarded to the in-cluster session manager as a kind 24210 request over
//! the Buzz relay.
//!
//! The provider acts as the agent: it signs with the payload's
//! `private_key_nsec` and is admitted to the relay through the payload's
//! NIP-OA `auth_tag`. It never needs the owner key or cluster credentials.
//! `info` and every local refusal answer without contacting the relay.
//!
//! Nothing here logs. Refusals name the failing step and never quote a relay
//! message, a payload field or the manager's reply envelope.

#![deny(unsafe_code)]

pub mod config;

use std::time::Duration;

use buzz_backend_kubernetes::provider::{self, check_sandbox_request};
use buzz_backend_kubernetes::wire::{
    ErrorCode, InfoResponse, ProviderError, Request, Response, OPS, PROTOCOL_VERSION,
};
use buzz_session_control::{build_request, open_reply, Op, Request as Envelope, Status, KIND};
use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Keys, Timestamp};
use serde_json::Value;

use crate::config::RelayConfig;

/// How long the manager has to accept a request before it counts as down.
pub const ACCEPT_WITHIN: Duration = Duration::from_secs(30);
/// Bound on connecting, authenticating and subscribing to replies.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest a stop may wait, as buzz-backend-kubernetes waits; under the
/// GPUI host's 150-second End deadline, so the client sees this provider's
/// own timeout rather than its own.
pub const STOP_WITHIN: Duration = Duration::from_secs(120);

/// How long `op` may take: `timeout_seconds`, but never more than
/// [`STOP_WITHIN`] for a stop.
fn time_limit(op: Op, configured: Duration) -> Duration {
    match op {
        Op::Stop => configured.min(STOP_WITHIN),
        _ => configured,
    }
}
const SUBSCRIPTION: &str = "session-control-replies";

/// The provider's self-description. Pure: no relay contact.
pub fn info() -> Response {
    Response::Info(InfoResponse {
        ok: true,
        name: "relay",
        version: env!("CARGO_PKG_VERSION"),
        protocol_version: PROTOCOL_VERSION,
        ops: OPS,
        description:
            "Runs agents in Agent Sandboxes through a session manager reached over the Buzz relay",
        config_schema: config::config_schema(),
    })
}

/// Produce the single response for one stdin request.
pub fn respond(input: &str) -> Value {
    let raw: Value = match serde_json::from_str(input) {
        Ok(raw) => raw,
        Err(_) => return value(Response::error("request is not valid JSON")),
    };
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(run(raw, ACCEPT_WITHIN)),
        Err(_) => value(Response::error("could not start the runtime")),
    }
}

/// Run one request to its single response. A forwarded request's response
/// is the manager's, verbatim; `accept_within` bounds the wait for the
/// manager to accept it.
pub async fn run(raw: Value, accept_within: Duration) -> Value {
    let request = match provider::parse(raw.clone()) {
        Ok(request) => request,
        Err(refusal) => return value(refusal),
    };
    let (op, request) = match request {
        Request::Info => return value(info()),
        Request::Deploy(request) => (Op::Deploy, request),
        Request::Stop(request) => (Op::Stop, request),
    };
    let checked = config::parse(&request.provider_config)
        .map_err(ProviderError::from)
        .and_then(|config| check_sandbox_request(&request).map(|()| config));
    let config = match checked {
        Ok(config) => config,
        Err(error) => return value(Response::failed(error)),
    };
    let keys = match Keys::parse(&request.agent.private_key_nsec) {
        Ok(keys) => keys,
        Err(_) => return value(Response::error("the agent private key is invalid")),
    };
    // Verified by the identity check above.
    let auth_tag = request.agent.auth_tag.clone().unwrap_or_default();
    let exchange = Exchange {
        config: &config,
        keys: &keys,
        auth_tag: &auth_tag,
        accept_within,
    };
    match exchange.run(op, forwarded(raw)).await {
        Ok(response) => response,
        Err(error) => value(Response::failed(error)),
    }
}

/// The payload the manager receives: the stdin request, with only the
/// `provider_config` fields the manager reads.
fn forwarded(mut raw: Value) -> Value {
    let config = raw
        .get("provider_config")
        .and_then(Value::as_object)
        .map(|config| {
            config
                .iter()
                .filter(|(field, _)| config::FORWARDED.contains(&field.as_str()))
                .map(|(field, value)| (field.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    raw["provider_config"] = Value::Object(config);
    raw
}

fn value(response: Response) -> Value {
    serde_json::to_value(&response).unwrap_or_else(
        |_| serde_json::json!({"ok": false, "error": "could not serialize a response"}),
    )
}

fn refused(code: ErrorCode, message: &str) -> ProviderError {
    ProviderError::refused(code, message)
}

/// One request's exchange with the manager.
struct Exchange<'a> {
    config: &'a RelayConfig,
    keys: &'a Keys,
    auth_tag: &'a str,
    accept_within: Duration,
}

impl Exchange<'_> {
    async fn run(&self, op: Op, payload: Value) -> Result<Value, ProviderError> {
        let started = tokio::time::Instant::now();
        let mut connection = tokio::time::timeout(CONNECT_TIMEOUT, self.connect())
            .await
            .unwrap_or(Err(WsClientError::Timeout))
            .map_err(|_| {
                refused(
                    ErrorCode::RelayUnavailable,
                    "could not connect, authenticate or subscribe to the relay",
                )
            })?;
        let response = self.exchange(&mut connection, op, payload, started).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), connection.disconnect()).await;
        response
    }

    /// Connect as the agent and subscribe to replies addressed to it. The
    /// subscription is confirmed before the request is sent: ephemeral
    /// replies reach only subscriptions that already exist.
    async fn connect(&self) -> Result<NostrWsConnection, WsClientError> {
        let tag = buzz_sdk::nip_oa::parse_auth_tag(self.auth_tag)
            .map_err(|_| WsClientError::AuthFailed(String::new()))?;
        let mut connection =
            NostrWsConnection::connect_authenticated(&self.config.relay_url, self.keys, Some(&tag))
                .await?;
        let filter = serde_json::json!({
            "kinds": [KIND], "#p": [self.keys.public_key().to_hex()], "limit": 0
        });
        connection
            .send_raw(&serde_json::json!(["REQ", SUBSCRIPTION, filter]))
            .await?;
        loop {
            match connection.next_event(CONNECT_TIMEOUT).await? {
                RelayMessage::Eose { subscription_id } if subscription_id == SUBSCRIPTION => {
                    return Ok(connection)
                }
                RelayMessage::Closed {
                    subscription_id, ..
                } if subscription_id == SUBSCRIPTION => {
                    return Err(WsClientError::ConnectionClosed)
                }
                _ => {}
            }
        }
    }

    /// Publish the request and wait for its `done` reply.
    async fn exchange(
        &self,
        connection: &mut NostrWsConnection,
        op: Op,
        payload: Value,
        started: tokio::time::Instant,
    ) -> Result<Value, ProviderError> {
        let lost = || {
            refused(
                ErrorCode::RelayUnavailable,
                "lost the relay connection before the session manager answered",
            )
        };
        let request_id = payload
            .get("request_id")
            .and_then(Value::as_str)
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= buzz_session_control::MAX_REQUEST_ID_BYTES
                    && id.bytes().all(|b| b.is_ascii_graphic())
            })
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string);
        let envelope = Envelope::new(op, request_id, payload);
        let event = build_request(
            self.keys,
            &self.config.manager,
            Some(self.auth_tag),
            &envelope,
            Timestamp::now(),
        )
        .map_err(|_| ProviderError::from("could not build the session request"))?;
        drop(envelope);
        let request_event = event.id;
        let ok = connection.send_event(event).await.map_err(|_| lost())?;
        if !ok.accepted {
            return Err(refused(
                ErrorCode::RelayUnavailable,
                "the relay refused the session request",
            ));
        }
        let accept_by = tokio::time::Instant::now() + self.accept_within;
        let limit = time_limit(op, self.config.timeout);
        let done_by = started + limit;
        let mut accepted = false;
        loop {
            let wait_until = if accepted {
                done_by
            } else {
                accept_by.min(done_by)
            };
            let now = tokio::time::Instant::now();
            if now >= wait_until {
                return Err(if accepted {
                    refused(
                        ErrorCode::ManagerTimeout,
                        &format!(
                            "the session manager did not finish the request within {} seconds",
                            limit.as_secs()
                        ),
                    )
                } else {
                    refused(
                        ErrorCode::ManagerUnavailable,
                        "the session manager did not accept the request; it may be down",
                    )
                });
            }
            let event = match connection.next_event(wait_until - now).await {
                Ok(RelayMessage::Event {
                    subscription_id,
                    event,
                }) if subscription_id == SUBSCRIPTION => event,
                Ok(RelayMessage::Closed {
                    subscription_id, ..
                }) if subscription_id == SUBSCRIPTION => return Err(lost()),
                Ok(_) | Err(WsClientError::Timeout) => continue,
                // One malformed frame does not end the exchange.
                Err(WsClientError::Json(_) | WsClientError::UnexpectedMessage(_)) => continue,
                Err(_) => return Err(lost()),
            };
            // Anything that is not the manager's reply to this request is
            // someone else's event; it is skipped, never surfaced.
            let Ok(reply) = open_reply(&event, self.keys, &self.config.manager, &request_event)
            else {
                continue;
            };
            match (reply.status, reply.response) {
                (Status::Accepted | Status::Progress, _) => accepted = true,
                (Status::Done, Some(response))
                    if response.get("ok").is_some_and(Value::is_boolean) =>
                {
                    return Ok(response)
                }
                (Status::Done, _) => {
                    return Err("the session manager sent a malformed response".into())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
