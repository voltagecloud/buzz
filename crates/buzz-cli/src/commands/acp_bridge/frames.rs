//! Agent observer frames (kind 24200): accepting telemetry from the driven
//! agent, mapping it to bridge signals, and building cancel control frames.
//!
//! The harness side lives in `crates/buzz-acp`: `observer.rs` defines the
//! plaintext `ObserverEvent`, `lib.rs` publishes it (single events or a
//! `batch` envelope) and handles `cancel_turn` control frames.

use buzz_core::kind::KIND_AGENT_OBSERVER_FRAME;
use buzz_core::observer::{
    decrypt_observer_payload, encrypt_observer_payload, OBSERVER_AGENT_TAG, OBSERVER_FRAME_CONTROL,
    OBSERVER_FRAME_TAG, OBSERVER_FRAME_TELEMETRY,
};
use nostr::{Event, Keys, PublicKey};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::CliError;

/// Observer kind of the harness's batch envelope (`payload.events`).
const BATCH_KIND: &str = "batch";

/// Most inner events read from one batch envelope.
const MAX_BATCH_EVENTS: usize = 1024;

/// `session/update` kinds forwarded to the ACP client. The remote adapter's
/// prompt echo (`user_message_chunk`, which includes harness framing) and its
/// commands and modes (not usable through the bridge) are not forwarded.
/// `usage_update` is forwarded separately, after [`usage_update`] rebuilds it.
const FORWARDED_UPDATES: &[&str] = &[
    "agent_message_chunk",
    "agent_thought_chunk",
    "tool_call",
    "tool_call_update",
    "plan",
];

/// ACP `Usage` token counts the bridge passes through from the remote
/// adapter's prompt response: (field, required by the ACP schema).
const USAGE_FIELDS: &[(&str, bool)] = &[
    ("inputTokens", true),
    ("outputTokens", true),
    ("totalTokens", true),
    ("thoughtTokens", false),
    ("cachedReadTokens", false),
    ("cachedWriteTokens", false),
];

/// Longest `usage_update.cost.currency` passed through (ISO 4217 codes are 3).
const MAX_CURRENCY_LEN: usize = 16;

/// ACP stop reasons passed through from the remote adapter's prompt response.
const STOP_REASONS: &[&str] = &[
    "end_turn",
    "max_tokens",
    "max_turn_requests",
    "refusal",
    "cancelled",
];

/// Identity and channel a bridge accepts frames for.
#[derive(Clone)]
pub(crate) struct Scope {
    /// The bridge signer: posts prompts, decrypts telemetry, signs control frames.
    pub signer: Keys,
    /// The human owner; this field selects history and confers no authorization.
    pub owner: PublicKey,
    /// The driven agent.
    pub agent: PublicKey,
    /// The channel the bridge posts in.
    pub channel: Uuid,
    /// The daemon's home directory, which holds the Paseo projects
    /// (see [`super::projects`]).
    pub home: std::path::PathBuf,
}

/// The fields of buzz-acp's `ObserverEvent` the bridge reads.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ObserverEvent {
    pub kind: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

/// Why a kind 24200 frame was not accepted.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// Not a telemetry frame (for example, a control frame).
    NotTelemetry,
    /// Not signed by, or not tagged for, the configured agent.
    WrongAgent,
    /// Addressed to someone other than the bridge signer.
    NotForSigner,
    BadSignature,
    Decrypt(String),
}

fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let parts = tag.as_slice();
        (parts.first().map(String::as_str) == Some(name))
            .then(|| parts.get(1).map(String::as_str))
            .flatten()
    })
}

/// Accept a telemetry frame from the configured agent and return its observer
/// events, with batch envelopes flattened. Events that name a different
/// channel are dropped; events without a channel are kept.
pub(crate) fn decode_telemetry(
    scope: &Scope,
    event: &Event,
) -> Result<Vec<ObserverEvent>, FrameError> {
    if u32::from(event.kind.as_u16()) != KIND_AGENT_OBSERVER_FRAME
        || tag_value(event, OBSERVER_FRAME_TAG) != Some(OBSERVER_FRAME_TELEMETRY)
    {
        return Err(FrameError::NotTelemetry);
    }
    let agent_hex = scope.agent.to_hex();
    if event.pubkey != scope.agent || tag_value(event, OBSERVER_AGENT_TAG) != Some(&agent_hex) {
        return Err(FrameError::WrongAgent);
    }
    if tag_value(event, "p") != Some(&scope.signer.public_key().to_hex()) {
        return Err(FrameError::NotForSigner);
    }
    event.verify().map_err(|_| FrameError::BadSignature)?;
    let outer: ObserverEvent = decrypt_observer_payload(&scope.signer, event)
        .map_err(|e| FrameError::Decrypt(e.to_string()))?;

    let mut events = Vec::new();
    if outer.kind == BATCH_KIND {
        let inner = outer
            .payload
            .get("events")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for value in inner.iter().take(MAX_BATCH_EVENTS) {
            match serde_json::from_value::<ObserverEvent>(value.clone()) {
                Ok(event) if event.kind != BATCH_KIND => events.push(event),
                Ok(_) => eprintln!("acp-bridge: ignored a nested observer batch"),
                Err(e) => eprintln!("acp-bridge: ignored a malformed observer event: {e}"),
            }
        }
        if inner.len() > MAX_BATCH_EVENTS {
            eprintln!(
                "acp-bridge: observer batch has {} events; read the first {MAX_BATCH_EVENTS}",
                inner.len()
            );
        }
    } else {
        events.push(outer);
    }
    events.retain(|event| match event.channel_id.as_deref() {
        None => true,
        Some(channel) => channel.parse::<Uuid>().ok() == Some(scope.channel),
    });
    Ok(events)
}

/// What one observer event means to the bridge.
#[derive(Debug, PartialEq)]
pub(crate) enum Signal {
    /// A turn began; `triggering` lists the events that prompted it.
    TurnStarted {
        turn_id: String,
        triggering: Vec<String>,
    },
    /// The turn ended (emitted on every harness exit path).
    TurnCompleted { turn_id: String },
    /// Periodic proof that the turn is still running.
    Liveness { turn_id: String },
    /// A `session/update` to forward to the ACP client.
    Update { turn_id: String, update: Value },
    /// The remote adapter answered its `session/prompt` with this stop reason
    /// and, when it reported valid token counts, this ACP `Usage`.
    StopReason {
        turn_id: String,
        reason: String,
        usage: Option<Value>,
    },
    /// The harness's answer to a `cancel_turn` control frame.
    CancelResult { request_id: String, status: String },
    /// Anything the bridge does not act on.
    Ignored,
}

/// Map one accepted observer event to a [`Signal`].
pub(crate) fn signal_of(event: ObserverEvent) -> Signal {
    let payload = event.payload;
    if event.kind == "control_result" {
        return match (
            payload.get("type").and_then(Value::as_str),
            payload.get("requestId").and_then(Value::as_str),
            payload.get("status").and_then(Value::as_str),
        ) {
            (Some("cancel_turn"), Some(request_id), Some(status)) => Signal::CancelResult {
                request_id: request_id.to_owned(),
                status: status.to_owned(),
            },
            _ => Signal::Ignored,
        };
    }
    let Some(turn_id) = event.turn_id else {
        return Signal::Ignored;
    };
    match event.kind.as_str() {
        "turn_started" => Signal::TurnStarted {
            turn_id,
            triggering: payload
                .get("triggeringEventIds")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        },
        "turn_completed" => Signal::TurnCompleted { turn_id },
        "turn_liveness" => Signal::Liveness { turn_id },
        "acp_read" => acp_read_signal(turn_id, payload),
        _ => Signal::Ignored,
    }
}

/// Map the remote adapter's raw ACP JSON-RPC message.
fn acp_read_signal(turn_id: String, mut message: Value) -> Signal {
    if message.get("method").and_then(Value::as_str) == Some("session/update") {
        let update = message
            .get_mut("params")
            .and_then(|params| params.get_mut("update"))
            .map(Value::take)
            .unwrap_or_default();
        let update = match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("usage_update") => usage_update(&update),
            Some(kind) if FORWARDED_UPDATES.contains(&kind) => Some(update),
            _ => None,
        };
        return match update {
            Some(update) => Signal::Update { turn_id, update },
            None => Signal::Ignored,
        };
    }
    let Some(result) = message.get("result") else {
        return Signal::Ignored;
    };
    match result.get("stopReason").and_then(Value::as_str) {
        Some(reason) if STOP_REASONS.contains(&reason) => Signal::StopReason {
            turn_id,
            reason: reason.to_owned(),
            usage: result.get("usage").and_then(prompt_usage),
        },
        _ => Signal::Ignored,
    }
}

/// A token count: a JSON integer in `0..=u64::MAX`.
fn token_count(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

/// Rebuild a `usage_update` from its validated fields: `size` (the context
/// window, > 0) and `used` (tokens in context), plus `cost` when it has a
/// finite `amount` and a short `currency`. Any other field is dropped, and the
/// update is dropped when `size` or `used` is invalid.
fn usage_update(update: &Value) -> Option<Value> {
    let size = token_count(update.get("size")).filter(|size| *size > 0)?;
    let used = token_count(update.get("used"))?;
    let mut rebuilt = json!({"sessionUpdate": "usage_update", "size": size, "used": used});
    let cost = update.get("cost");
    let amount = cost
        .and_then(|cost| cost.get("amount"))
        .and_then(Value::as_f64)
        .filter(|amount| amount.is_finite());
    let currency = cost
        .and_then(|cost| cost.get("currency"))
        .and_then(Value::as_str)
        .filter(|currency| !currency.is_empty() && currency.len() <= MAX_CURRENCY_LEN);
    if let (Some(amount), Some(currency)) = (amount, currency) {
        rebuilt["cost"] = json!({"amount": amount, "currency": currency});
    }
    Some(rebuilt)
}

/// Rebuild a prompt response's ACP `Usage` from its known token counts.
/// `None` when a count the ACP schema requires is missing or invalid, so the
/// client never sees a partial or zero-filled `Usage`.
fn prompt_usage(usage: &Value) -> Option<Value> {
    let mut rebuilt = serde_json::Map::new();
    for &(field, required) in USAGE_FIELDS {
        match token_count(usage.get(field)) {
            Some(count) => {
                rebuilt.insert(field.to_owned(), count.into());
            }
            None if required => return None,
            None => {}
        }
    }
    Some(Value::Object(rebuilt))
}

/// Build a signed `cancel_turn` control frame for the agent's turn in
/// `scope.channel`.
///
/// Matches what `handle_relay_observer_control_event` and
/// `handle_cancel_turn_control` in `crates/buzz-acp/src/lib.rs` accept: kind
/// 24200 with `p` and `agent` set to the agent, `frame` = `control`, content
/// NIP-44 encrypted to the agent, a `channelId`, and a `requestId` the harness
/// echoes in its `control_result`. The harness only accepts frames signed by
/// the agent's owner and created within five minutes of its clock, so the
/// frame is stamped now and must be published promptly.
pub(crate) fn cancel_turn_frame(scope: &Scope, request_id: &str) -> Result<Event, CliError> {
    let payload = json!({
        "type": "cancel_turn",
        "channelId": scope.channel.to_string(),
        "requestId": request_id,
    });
    let encrypted = encrypt_observer_payload(&scope.signer, &scope.agent, &payload)
        .map_err(|e| CliError::Other(format!("cancel frame encryption failed: {e}")))?;
    let agent_hex = scope.agent.to_hex();
    buzz_sdk::build_agent_observer_frame(&agent_hex, &agent_hex, OBSERVER_FRAME_CONTROL, &encrypted)
        .map_err(|e| CliError::Other(format!("cancel frame build failed: {e}")))?
        .sign_with_keys(&scope.signer)
        .map_err(|e| CliError::Other(format!("cancel frame signing failed: {e}")))
}
