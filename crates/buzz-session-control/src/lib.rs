//! Kind 24210 "Sandbox session control": the request and reply events a
//! remote-agent provider and its session manager exchange through a Buzz
//! relay (`docs/remote-agents.md` §Relay transport).
//!
//! Confidentiality is structural. `content` is always NIP-44 v2 ciphertext:
//! the builders take typed envelopes and never content, and the openers
//! refuse anything that does not decrypt, plain JSON included. The only
//! clear-text tags are `p`, `e` (replies), `auth` (requests) and
//! `expiration`; the operation, request id, namespace and every payload
//! field stay inside the ciphertext.
//!
//! Decrypted plaintext is zeroized after parsing. The parsed
//! [`serde_json::Value`] payload is not, and its `Debug` output is redacted.

#![deny(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;

use buzz_core::kind::KIND_SANDBOX_SESSION_CONTROL;
use nostr::nips::nip44;
use nostr::{Event, EventBuilder, EventId, Keys, Kind, PublicKey, Tag, Timestamp};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// The event kind, as the `u16` that `nostr` uses.
pub const KIND: u16 = KIND_SANDBOX_SESSION_CONTROL as u16;
const _: () = assert!(KIND as u32 == KIND_SANDBOX_SESSION_CONTROL);

/// The only envelope version this crate reads and writes.
pub const VERSION: u32 = 1;
/// Largest accepted distance between an event's `created_at` and now.
pub const MAX_CREATED_AT_SKEW_SECS: u64 = 60;
/// Lifetime written into every event's NIP-40 `expiration` tag.
pub const EXPIRATION_SECS: u64 = 120;
/// Largest envelope plaintext: the NIP-44 v2 limit.
pub const MAX_PLAINTEXT_BYTES: usize = 65_535;
/// Longest accepted `request_id`.
pub const MAX_REQUEST_ID_BYTES: usize = 128;

/// A session operation. `info`, `deploy` and `stop` carry provider protocol
/// requests; recovery is a `deploy` with `provider_config.sandbox.recovery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// Describe the provider.
    Info,
    /// Start, resume or recover a Sandbox session.
    Deploy,
    /// End a Sandbox session.
    Stop,
    /// Store a developer's agent profile. Signed by the owner key itself.
    Enroll,
    /// Describe the developer's stored profile, naming no value. Signed by
    /// the owner key itself.
    Profile,
}

impl Op {
    /// The wire spelling, which is also the provider protocol `op`.
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Info => "info",
            Op::Deploy => "deploy",
            Op::Stop => "stop",
            Op::Enroll => "enroll",
            Op::Profile => "profile",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Op::Info, Op::Deploy, Op::Stop, Op::Enroll, Op::Profile]
            .into_iter()
            .find(|op| op.as_str() == value)
    }
}

/// Where a request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The manager authorized the request and started it.
    Accepted,
    /// A long operation is still running.
    Progress,
    /// Terminal: `response` holds the provider protocol response.
    Done,
}

/// The decrypted request envelope `{v, op, request_id, payload}`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Envelope version; always [`VERSION`] when built here.
    pub v: u32,
    /// The operation.
    pub op: Op,
    /// Caller-chosen idempotency key, stable across retries.
    pub request_id: String,
    /// The provider protocol request (today's stdin JSON), including secrets.
    pub payload: serde_json::Value,
}

impl Request {
    /// A current-version request.
    pub fn new(op: Op, request_id: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            v: VERSION,
            op,
            request_id: request_id.into(),
            payload,
        }
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("v", &self.v)
            .field("op", &self.op)
            .field("request_id", &self.request_id)
            .field("payload", &"<redacted>")
            .finish()
    }
}

/// The decrypted reply envelope `{v, request_id, status, response}`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// Envelope version; always [`VERSION`] when built here.
    pub v: u32,
    /// The request this answers.
    pub request_id: String,
    /// Where the request stands.
    pub status: Status,
    /// The provider protocol response; present when `status` is `done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<serde_json::Value>,
}

impl Reply {
    /// The request was authorized and started.
    pub fn accepted(request_id: impl Into<String>) -> Self {
        Self::new(request_id, Status::Accepted, None)
    }

    /// The request is still running.
    pub fn progress(request_id: impl Into<String>) -> Self {
        Self::new(request_id, Status::Progress, None)
    }

    /// The terminal reply, carrying a provider protocol response or refusal.
    pub fn done(request_id: impl Into<String>, response: serde_json::Value) -> Self {
        Self::new(request_id, Status::Done, Some(response))
    }

    fn new(
        request_id: impl Into<String>,
        status: Status,
        response: Option<serde_json::Value>,
    ) -> Self {
        Self {
            v: VERSION,
            request_id: request_id.into(),
            status,
            response,
        }
    }
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("v", &self.v)
            .field("request_id", &self.request_id)
            .field("status", &self.status)
            .field("response", &self.response.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A request event that verified, was addressed to the opening key, and
/// decrypted to a current-version envelope.
#[derive(Debug, Clone)]
pub struct OpenedRequest {
    /// The request event id; replies carry it in their `e` tag.
    pub event_id: EventId,
    /// The event author: the agent key, or the owner key for `enroll`.
    pub signer: PublicKey,
    /// The event's `created_at`.
    pub created_at: Timestamp,
    /// The event's NIP-40 `expiration`, if any.
    pub expiration: Option<Timestamp>,
    /// The NIP-OA `auth` tag as the JSON array `buzz_sdk::nip_oa` verifies.
    /// Structurally valid; its signature is the caller's to verify.
    pub auth_tag: Option<String>,
    /// The decrypted envelope.
    pub request: Request,
}

impl OpenedRequest {
    /// Refuse a request outside [`MAX_CREATED_AT_SKEW_SECS`] of `now`, or one
    /// whose `expiration` has passed.
    pub fn check_fresh(&self, now: Timestamp) -> Result<(), WireError> {
        check_created_at(self.created_at, now)?;
        match self.expiration {
            Some(expiration) if expiration <= now => Err(WireError::Stale),
            _ => Ok(()),
        }
    }
}

/// Why an event could not be built or opened. `Display` never includes
/// content, payload fields or tag values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// The event is not kind 24210.
    #[error("event is not a session control event")]
    WrongKind,
    /// The event id or signature does not verify.
    #[error("event id or signature is invalid")]
    BadSignature,
    /// The `p` tag does not name the opening key.
    #[error("event is not addressed to this key")]
    WrongRecipient,
    /// A reply was not signed by the expected manager.
    #[error("reply is not from the session manager")]
    WrongSender,
    /// A reply's `e` tag names a different request event.
    #[error("reply answers a different request")]
    WrongRequest,
    /// A tag outside the allowed set, a duplicate, or a malformed allowed tag.
    #[error("event carries a tag outside the allowed set")]
    UnexpectedTag,
    /// The `auth` tag is not a structurally valid NIP-OA attestation.
    #[error("auth tag is not a valid owner attestation")]
    MalformedAuthTag,
    /// `content` is not NIP-44 v2 ciphertext (for example, plain JSON).
    #[error("content is not NIP-44 v2 ciphertext")]
    NotCiphertext,
    /// `content` is NIP-44 v2 ciphertext for a different key, or tampered.
    #[error("content does not decrypt with this key")]
    Undecryptable,
    /// The plaintext is not a valid envelope.
    #[error("envelope is malformed")]
    MalformedEnvelope,
    /// The envelope version is not [`VERSION`].
    #[error("unsupported envelope version {version}")]
    UnsupportedVersion {
        /// The version the sender used.
        version: u32,
        /// The envelope's request id, so a refusal can answer it.
        request_id: String,
    },
    /// The request's `op` is not an [`Op`].
    #[error("unsupported operation")]
    UnsupportedOp {
        /// The envelope's request id, so a refusal can answer it.
        request_id: String,
    },
    /// The envelope exceeds [`MAX_PLAINTEXT_BYTES`].
    #[error("envelope exceeds the NIP-44 plaintext limit")]
    TooLarge,
    /// `created_at` is outside the allowed skew, or `expiration` has passed.
    #[error("event is stale or from the future")]
    Stale,
    /// Encryption failed.
    #[error("could not encrypt the envelope")]
    Encrypt,
    /// Signing failed.
    #[error("could not sign the event")]
    Sign,
}

impl WireError {
    /// Stable snake_case label, safe to log.
    pub fn label(&self) -> &'static str {
        match self {
            WireError::WrongKind => "wrong_kind",
            WireError::BadSignature => "bad_signature",
            WireError::WrongRecipient => "wrong_recipient",
            WireError::WrongSender => "wrong_sender",
            WireError::WrongRequest => "wrong_request",
            WireError::UnexpectedTag => "unexpected_tag",
            WireError::MalformedAuthTag => "malformed_auth_tag",
            WireError::NotCiphertext => "not_ciphertext",
            WireError::Undecryptable => "undecryptable",
            WireError::MalformedEnvelope => "malformed_envelope",
            WireError::UnsupportedVersion { .. } => "unsupported_version",
            WireError::UnsupportedOp { .. } => "unsupported_op",
            WireError::TooLarge => "too_large",
            WireError::Stale => "stale",
            WireError::Encrypt => "encrypt",
            WireError::Sign => "sign",
        }
    }
}

/// Refuse a `created_at` more than [`MAX_CREATED_AT_SKEW_SECS`] from `now`.
pub fn check_created_at(created_at: Timestamp, now: Timestamp) -> Result<(), WireError> {
    if created_at.as_secs().abs_diff(now.as_secs()) > MAX_CREATED_AT_SKEW_SECS {
        return Err(WireError::Stale);
    }
    Ok(())
}

/// Encrypt `request` to `manager` and sign it with `signer`.
///
/// `auth_tag` is the owner's NIP-OA attestation JSON for the signing agent
/// key (`BUZZ_AUTH_TAG`); `enroll` is signed by the owner and passes `None`.
pub fn build_request(
    signer: &Keys,
    manager: &PublicKey,
    auth_tag: Option<&str>,
    request: &Request,
    created_at: Timestamp,
) -> Result<Event, WireError> {
    let mut tags = vec![Tag::public_key(*manager)];
    if let Some(auth_tag) = auth_tag {
        tags.push(
            buzz_sdk::nip_oa::parse_auth_tag(auth_tag).map_err(|_| WireError::MalformedAuthTag)?,
        );
    }
    seal(signer, manager, request, tags, created_at)
}

/// Encrypt `reply` to `requester` and sign it with the manager key.
pub fn build_reply(
    manager: &Keys,
    requester: &PublicKey,
    request_event: &EventId,
    reply: &Reply,
    created_at: Timestamp,
) -> Result<Event, WireError> {
    let tags = vec![Tag::public_key(*requester), Tag::event(*request_event)];
    seal(manager, requester, reply, tags, created_at)
}

/// Verify, check addressing and tags, and decrypt a request for `manager`.
pub fn open_request(event: &Event, manager: &Keys) -> Result<OpenedRequest, WireError> {
    let tags = Tags::of(event)?;
    if tags.p != Some(manager.public_key()) {
        return Err(WireError::WrongRecipient);
    }
    if tags.e.is_some() {
        return Err(WireError::UnexpectedTag);
    }
    let value = unseal(event, manager)?;
    let request_id = header(&value)?;
    let mut fields = match value {
        serde_json::Value::Object(fields) => fields,
        _ => return Err(WireError::MalformedEnvelope),
    };
    let op = match fields.get("op") {
        Some(serde_json::Value::String(op)) => Op::parse(op).ok_or(WireError::UnsupportedOp {
            request_id: request_id.clone(),
        })?,
        _ => return Err(WireError::MalformedEnvelope),
    };
    let payload = fields
        .remove("payload")
        .ok_or(WireError::MalformedEnvelope)?;
    Ok(OpenedRequest {
        event_id: event.id,
        signer: event.pubkey,
        created_at: event.created_at,
        expiration: tags.expiration,
        auth_tag: tags.auth,
        request: Request {
            v: VERSION,
            op,
            request_id,
            payload,
        },
    })
}

/// Verify, check addressing and tags, and decrypt a reply from `manager` to
/// the request event `request_event`.
pub fn open_reply(
    event: &Event,
    requester: &Keys,
    manager: &PublicKey,
    request_event: &EventId,
) -> Result<Reply, WireError> {
    let tags = Tags::of(event)?;
    if event.pubkey != *manager {
        return Err(WireError::WrongSender);
    }
    if tags.p != Some(requester.public_key()) {
        return Err(WireError::WrongRecipient);
    }
    if tags.auth.is_some() {
        return Err(WireError::UnexpectedTag);
    }
    if tags.e != Some(*request_event) {
        return Err(WireError::WrongRequest);
    }
    let value = unseal(event, requester)?;
    header(&value)?;
    serde_json::from_value(value).map_err(|_| WireError::MalformedEnvelope)
}

/// The one encrypt-and-sign path. Private, so no caller can supply content.
fn seal<T: Serialize>(
    signer: &Keys,
    recipient: &PublicKey,
    envelope: &T,
    mut tags: Vec<Tag>,
    created_at: Timestamp,
) -> Result<Event, WireError> {
    let mut plaintext = serde_json::to_string(envelope).map_err(|_| WireError::Encrypt)?;
    if plaintext.len() > MAX_PLAINTEXT_BYTES {
        plaintext.zeroize();
        return Err(WireError::TooLarge);
    }
    let ciphertext = nip44::encrypt(
        signer.secret_key(),
        recipient,
        &plaintext,
        nip44::Version::V2,
    );
    plaintext.zeroize();
    let ciphertext = ciphertext.map_err(|_| WireError::Encrypt)?;
    tags.push(Tag::expiration(Timestamp::from(
        created_at.as_secs().saturating_add(EXPIRATION_SECS),
    )));
    EventBuilder::new(Kind::Custom(KIND), ciphertext)
        .tags(tags)
        .custom_created_at(created_at)
        .sign_with_keys(signer)
        .map_err(|_| WireError::Sign)
}

/// Decrypt a verified event's content with `recipient`'s key.
fn unseal(event: &Event, recipient: &Keys) -> Result<serde_json::Value, WireError> {
    if !buzz_core::observer::content_looks_like_nip44(&event.content) {
        return Err(WireError::NotCiphertext);
    }
    let mut plaintext = nip44::decrypt(recipient.secret_key(), &event.pubkey, &event.content)
        .map_err(|error| match error {
            nip44::Error::Base64Decode(_)
            | nip44::Error::UnknownVersion(_)
            | nip44::Error::VersionNotFound => WireError::NotCiphertext,
            _ => WireError::Undecryptable,
        })?;
    let parsed = if plaintext.len() > MAX_PLAINTEXT_BYTES {
        Err(WireError::TooLarge)
    } else {
        serde_json::from_str(&plaintext).map_err(|_| WireError::MalformedEnvelope)
    };
    plaintext.zeroize();
    parsed
}

/// Check the envelope version and return its request id. Read before the
/// rest of the envelope so a refusal can answer an unknown version.
fn header(value: &serde_json::Value) -> Result<String, WireError> {
    let request_id = match value.get("request_id") {
        Some(serde_json::Value::String(id))
            if !id.is_empty()
                && id.len() <= MAX_REQUEST_ID_BYTES
                && id.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            id.clone()
        }
        _ => return Err(WireError::MalformedEnvelope),
    };
    match value.get("v").and_then(serde_json::Value::as_u64) {
        Some(v) if v == u64::from(VERSION) => Ok(request_id),
        Some(v) => Err(WireError::UnsupportedVersion {
            version: u32::try_from(v).unwrap_or(u32::MAX),
            request_id,
        }),
        None => Err(WireError::MalformedEnvelope),
    }
}

/// The clear-text tags of a session control event. Anything else, and any
/// duplicate, is refused: descriptive data belongs inside the ciphertext.
#[derive(Default)]
struct Tags {
    p: Option<PublicKey>,
    e: Option<EventId>,
    auth: Option<String>,
    expiration: Option<Timestamp>,
}

impl Tags {
    /// Check the kind and the signature, then read the tags.
    fn of(event: &Event) -> Result<Self, WireError> {
        if event.kind != Kind::Custom(KIND) {
            return Err(WireError::WrongKind);
        }
        if event.verify().is_err() {
            return Err(WireError::BadSignature);
        }
        let mut tags = Tags::default();
        for tag in event.tags.iter() {
            match tag.as_slice() {
                [name, value] if name == "p" && tags.p.is_none() => {
                    tags.p =
                        Some(PublicKey::from_hex(value).map_err(|_| WireError::UnexpectedTag)?);
                }
                [name, value] if name == "e" && tags.e.is_none() => {
                    tags.e = Some(EventId::from_hex(value).map_err(|_| WireError::UnexpectedTag)?);
                }
                [name, value] if name == "expiration" && tags.expiration.is_none() => {
                    let seconds = value.parse::<u64>().map_err(|_| WireError::UnexpectedTag)?;
                    tags.expiration = Some(Timestamp::from(seconds));
                }
                [name, ..] if name == "auth" && tags.auth.is_none() => {
                    let json = serde_json::to_string(tag.as_slice())
                        .map_err(|_| WireError::MalformedAuthTag)?;
                    buzz_sdk::nip_oa::parse_auth_tag(&json)
                        .map_err(|_| WireError::MalformedAuthTag)?;
                    tags.auth = Some(json);
                }
                _ => return Err(WireError::UnexpectedTag),
            }
        }
        Ok(tags)
    }
}

#[cfg(test)]
mod tests;
