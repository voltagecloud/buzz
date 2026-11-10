//! Owner-authorized terminal attachment carried only by relay events.
//!
//! This module defines the wire boundary. Consumers must read current grants
//! and delegations through a strong relay query before opening a terminal and
//! periodically while it remains open. No shell or network access occurs here.

use std::collections::VecDeque;

use base64::{engine::general_purpose::STANDARD, Engine};
use buzz_core::observer::{decrypt_observer_payload, encrypt_observer_payload};
use nostr::{Event, EventBuilder, EventId, Keys, Kind, PublicKey, Tag, Timestamp};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{kind, SdkError};

/// Separately approved, encrypted read-only workspace snapshots.
pub mod snapshot;

/// Longest terminal grant lifetime, in seconds.
pub const MAX_GRANT_SECS: u64 = 3600;
/// Largest decoded data frame, in bytes.
pub const MAX_DATA_BYTES: usize = 16 * 1024;
/// Frames retained for acknowledgement, per direction.
pub const MAX_IN_FLIGHT: usize = 4;
/// Maximum cumulative unacknowledged data bytes, per direction.
pub const MAX_IN_FLIGHT_BYTES: usize = MAX_IN_FLIGHT * MAX_DATA_BYTES;
/// Frames older than this many seconds are refused.
pub const MAX_FRAME_AGE_SECS: u64 = 15;
/// Delay before an unacknowledged frame is retried.
pub const RETRY_MS: u64 = 1000;
/// Retries before the transport reports a terminal failure.
pub const MAX_RETRIES: u8 = 5;
/// Largest frame rate, including retries and acknowledgements, per direction.
pub const MAX_FRAMES_PER_SECOND: u16 = 64;

/// Frame admission budget. Apply before cryptography; backpressure sends and drop excess input.
#[derive(Default)]
pub struct Rate {
    window_ms: u64,
    frames: u16,
}
impl Rate {
    /// Admit one frame using a monotonic elapsed-millisecond clock.
    pub fn allow(&mut self, now_ms: u64) -> bool {
        if now_ms.saturating_sub(self.window_ms) >= 1000 {
            self.window_ms = now_ms;
            self.frames = 0;
        }
        if self.frames >= MAX_FRAMES_PER_SECOND {
            return false;
        }
        self.frames += 1;
        true
    }
}

fn invalid(message: &str) -> SdkError {
    SdkError::InvalidInput(message.to_owned())
}

fn tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    let mut matches = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().is_some_and(|key| key == name));
    let value = matches.next()?.as_slice().get(1)?.as_str();
    matches.next().is_none().then_some(value)
}

fn tags(values: Vec<Vec<String>>) -> Result<Vec<Tag>, SdkError> {
    values
        .into_iter()
        .map(|value| Tag::parse(value).map_err(|_| invalid("invalid attach tag")))
        .collect()
}

/// Immutable sandbox identity; the harness compares all three fields to its binding.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// Channel containing the sandbox thread.
    pub channel: Uuid,
    /// Thread root event ID.
    pub thread: EventId,
    /// Remote agent's public key.
    pub agent: PublicKey,
}

impl Scope {
    fn tags(self, recipient: PublicKey) -> Vec<Vec<String>> {
        vec![
            vec!["h".into(), self.channel.to_string()],
            vec!["e".into(), self.thread.to_hex()],
            vec!["p".into(), recipient.to_hex()],
        ]
    }

    fn matches(self, event: &Event, recipient: PublicKey) -> bool {
        tag(event, "h") == Some(self.channel.to_string().as_str())
            && tag(event, "e") == Some(self.thread.to_hex().as_str())
            && tag(event, "p") == Some(recipient.to_hex().as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    mode: String,
    session: Uuid,
    scope: Scope,
    expires_at: u64,
    requester: PublicKey,
    request_id: Option<EventId>,
}

/// Current authority snapshot, fetched strongly from the same community relay.
pub struct Authority<'a> {
    /// Immutable sandbox owner.
    pub owner: PublicKey,
    /// Trusted community relay signing key.
    pub relay: PublicKey,
    /// Current relay-signed channel member roster.
    pub roster: &'a Event,
    /// Current owner-signed delegation heads.
    pub delegations: &'a [Event],
    /// Current candidate gateway profiles, used to exclude agent keys.
    pub profiles: &'a [Event],
}

/// Verified, bounded request metadata. Signature validation alone does not authorize a shell.
#[derive(Clone, Debug)]
pub struct Grant {
    /// Original owner-signed approval, retained in audit records.
    event: Event,
    /// Unique terminal session ID.
    session: Uuid,
    /// Exact sandbox identity.
    scope: Scope,
    /// Requesting identity explicitly named by the owner.
    requester: PublicKey,
    /// Authorization expiry in Unix seconds.
    expires_at: u64,
    request_id: EventId,
}

impl Grant {
    /// Original signed request metadata.
    pub fn event(&self) -> &Event {
        &self.event
    }
    /// Signed request ID binding every transport frame.
    pub fn id(&self) -> EventId {
        self.event.id
    }
    /// Unique terminal session ID.
    pub fn session(&self) -> Uuid {
        self.session
    }
    /// Immutable sandbox scope.
    pub fn scope(&self) -> Scope {
        self.scope
    }
    /// Requesting key; consumers must check current authority.
    pub fn requester(&self) -> PublicKey {
        self.requester
    }
    /// Authorization expiry, in Unix seconds.
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }
    /// Verify an owner-approval envelope and its one-hour-or-shorter lifetime.
    /// [`Self::authorized`] separately checks the expected owner and current authority.
    pub fn parse(event: &Event, scope: Scope, now: u64) -> Result<Self, SdkError> {
        let request = read_request(event, Some(scope), "grant", "terminal", now)?;
        let request_id = request
            .request_id
            .ok_or_else(|| invalid("terminal approval has no request"))?;
        Ok(Self {
            event: event.clone(),
            session: request.session,
            scope,
            requester: request.requester,
            expires_at: request.expires_at,
            request_id,
        })
    }

    /// Pending request this owner approved.
    pub fn request_id(&self) -> EventId {
        self.request_id
    }

    /// Check current posting membership and owner/live-gateway authority.
    /// The latest relay-signed roster and authority records must come from strong reads.
    pub fn authorized(&self, pending: &Pending, authority: Authority<'_>, now: u64) -> bool {
        self.event.pubkey == authority.owner
            && now < self.expires_at
            && self.request_id == pending.id()
            && self.session == pending.session()
            && self.scope == pending.scope()
            && self.requester == pending.requester()
            && self.expires_at <= pending.expires_at()
            && [authority.owner, self.scope.agent, self.requester]
                .iter()
                .all(|key| {
                    crate::gateway::channel_member(
                        authority.roster,
                        authority.relay,
                        self.scope.channel,
                        *key,
                    )
                })
            && self.requester != self.scope.agent
            && (self.requester == authority.owner
                || crate::gateway::is_live_delegate(
                    authority.owner,
                    self.requester,
                    authority.delegations,
                    authority.profiles,
                    now,
                ))
    }
}

/// A signed request awaiting an explicit per-session owner approval.
/// A request or a global gateway delegation can never authorize a shell by itself.
#[derive(Clone, Debug)]
pub struct Pending {
    event: Event,
    body: Request,
}

impl Pending {
    /// Verify a bounded request, without treating its sender as the sandbox owner.
    pub fn parse(event: &Event, now: u64) -> Result<Self, SdkError> {
        let body = read_request(event, None, "request", "terminal", now)?;
        if body.requester != event.pubkey || body.request_id.is_some() {
            return Err(invalid("invalid pending terminal requester"));
        }
        Ok(Self {
            event: event.clone(),
            body,
        })
    }
    /// Pending signed request ID.
    pub fn id(&self) -> EventId {
        self.event.id
    }
    /// Requester waiting for owner approval.
    pub fn requester(&self) -> PublicKey {
        self.body.requester
    }
    /// Requested immutable sandbox scope.
    pub fn scope(&self) -> Scope {
        self.body.scope
    }
    /// Requested session identifier.
    pub fn session(&self) -> Uuid {
        self.body.session
    }
    /// Latest possible approval expiry, in Unix seconds.
    pub fn expires_at(&self) -> u64 {
        self.body.expires_at
    }
}

fn read_request(
    event: &Event,
    scope: Option<Scope>,
    phase: &str,
    mode: &str,
    now: u64,
) -> Result<Request, SdkError> {
    if event.kind.as_u16() as u32 != kind::KIND_SANDBOX_ATTACH
        || event.verify().is_err()
        || event.content.len() > 2048
    {
        return Err(invalid("invalid terminal request"));
    }
    let request: Request =
        serde_json::from_str(&event.content).map_err(|_| invalid("invalid terminal request"))?;
    if request.version != 1
        || request.mode != mode
        || scope.is_some_and(|scope| scope != request.scope)
        || event.created_at.as_secs() > now
        || request.expires_at <= now
        || request
            .expires_at
            .saturating_sub(event.created_at.as_secs())
            > MAX_GRANT_SECS
        || tag(event, "expiration").and_then(|value| value.parse::<u64>().ok())
            != Some(request.expires_at)
        || tag(event, "d") != Some(format!("{mode}:{}:{phase}", request.session).as_str())
        || !request.scope.matches(event, request.scope.agent)
    {
        return Err(invalid("expired or incorrectly scoped terminal request"));
    }
    Ok(request)
}

fn signed_request(keys: &Keys, body: Request, phase: &str) -> Result<Event, SdkError> {
    let now = Timestamp::now().as_secs();
    if body.expires_at <= now || body.expires_at.saturating_sub(now) > MAX_GRANT_SECS {
        return Err(invalid("terminal expiry must be within one hour"));
    }
    let mut values = body.scope.tags(body.scope.agent);
    values.push(vec![
        "d".into(),
        format!("{}:{}:{phase}", body.mode, body.session),
    ]);
    values.push(vec!["expiration".into(), body.expires_at.to_string()]);
    let content = serde_json::to_string(&body).map_err(|_| invalid("invalid terminal request"))?;
    EventBuilder::new(Kind::from(kind::KIND_SANDBOX_ATTACH as u16), content)
        .tags(tags(values)?)
        .sign_with_keys(keys)
        .map_err(|_| invalid("could not sign terminal request"))
}

/// Publishable pending request. Only a separate owner-signed approval permits attachment.
pub fn request(
    keys: &Keys,
    scope: Scope,
    session: Uuid,
    expires_at: u64,
) -> Result<Event, SdkError> {
    signed_request(
        keys,
        Request {
            version: 1,
            mode: "terminal".into(),
            session,
            scope,
            expires_at,
            requester: keys.public_key(),
            request_id: None,
        },
        "request",
    )
}

/// Approve exactly this request with the owner's key, never beyond its requested expiry.
/// The harness checks the signature against its immutable owner identity.
pub fn approve(owner: &Keys, pending: &Pending, expires_at: u64) -> Result<Event, SdkError> {
    if expires_at > pending.expires_at() {
        return Err(invalid("approval exceeds requested expiry"));
    }
    signed_request(
        owner,
        Request {
            expires_at,
            request_id: Some(pending.id()),
            ..pending.body.clone()
        },
        "grant",
    )
}

/// Store acceptance/end metadata, including the original signed metadata-only grant.
/// `status` is either `accepted` or `ended`; no terminal bytes belong in this record.
pub fn audit(
    keys: &Keys,
    grant: &Grant,
    status: &str,
    bytes_in: u64,
    bytes_out: u64,
) -> Result<Event, SdkError> {
    if keys.public_key() != grant.scope.agent || !matches!(status, "accepted" | "ended") {
        return Err(invalid("invalid terminal audit signer or status"));
    }
    let content = serde_json::json!({"version":1,"mode":"terminal","session":grant.session,"status":status,"grant":grant.event,"bytes_in":bytes_in,"bytes_out":bytes_out}).to_string();
    let mut values = grant.scope.tags(grant.requester);
    values.push(vec![
        "d".into(),
        format!("terminal:{}:{status}", grant.session),
    ]);
    EventBuilder::new(Kind::from(kind::KIND_SANDBOX_ATTACH as u16), content)
        .tags(tags(values)?)
        .sign_with_keys(keys)
        .map_err(|_| invalid("could not sign terminal audit"))
}

/// Encrypted terminal message. Data is base64; acknowledgements contain no data.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Packet {
    /// Start the shell with terminal dimensions.
    Open {
        /// Terminal rows, 1–1000.
        rows: u16,
        /// Terminal columns, 1–1000.
        cols: u16,
    },
    /// Terminal input or output, at most [`MAX_DATA_BYTES`] decoded bytes.
    Data {
        /// Canonical base64-encoded bytes.
        data: String,
    },
    /// Change terminal dimensions.
    Resize {
        /// Terminal rows, 1–1000.
        rows: u16,
        /// Terminal columns, 1–1000.
        cols: u16,
    },
    /// Close the attachment.
    Close,
    /// Acknowledge the highest contiguous incoming sequence number.
    Ack,
}

impl std::fmt::Debug for Packet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Data { data } => f
                .debug_struct("Data")
                .field("encoded_bytes", &data.len())
                .finish(),
            Self::Open { rows, cols } => f.debug_tuple("Open").field(rows).field(cols).finish(),
            Self::Resize { rows, cols } => f.debug_tuple("Resize").field(rows).field(cols).finish(),
            Self::Close => f.write_str("Close"),
            Self::Ack => f.write_str("Ack"),
        }
    }
}

impl Packet {
    /// Encode bounded terminal bytes without interpreting their text.
    pub fn data(bytes: &[u8]) -> Result<Self, SdkError> {
        if bytes.len() > MAX_DATA_BYTES {
            return Err(invalid("terminal data exceeds 16 KiB"));
        }
        Ok(Self::Data {
            data: STANDARD.encode(bytes),
        })
    }

    /// Validate and decode a data packet; other packets return no bytes.
    pub fn bytes(&self) -> Result<Vec<u8>, SdkError> {
        match self {
            Self::Data { data } => {
                if data.len() > MAX_DATA_BYTES.div_ceil(3) * 4 {
                    return Err(invalid("terminal frame exceeds bound"));
                }
                let bytes = STANDARD
                    .decode(data)
                    .map_err(|_| invalid("invalid terminal base64"))?;
                if bytes.len() > MAX_DATA_BYTES || STANDARD.encode(&bytes) != *data {
                    return Err(invalid("invalid terminal data"));
                }
                Ok(bytes)
            }
            Self::Open { rows, cols } | Self::Resize { rows, cols }
                if !(1..=1000).contains(rows) || !(1..=1000).contains(cols) =>
            {
                Err(invalid("invalid terminal dimensions"))
            }
            _ => Ok(Vec::new()),
        }
    }
}

/// One NIP-44 encrypted transport frame, bound to an accepted grant.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    /// Owner-signed approval ID; frames cannot migrate between grants.
    pub grant: EventId,
    /// Data/control sequence, or zero for acknowledgement-only frames.
    pub seq: u64,
    /// Highest contiguous incoming sequence observed by the sender.
    pub ack: u64,
    /// Terminal operation.
    pub packet: Packet,
}

/// Encrypt a frame to its peer. Retries re-encrypt, creating a fresh Nostr event ID.
pub fn encrypt(
    keys: &Keys,
    peer: PublicKey,
    scope: Scope,
    frame: &Frame,
) -> Result<Event, SdkError> {
    frame.packet.bytes()?;
    if (frame.seq == 0) != matches!(frame.packet, Packet::Ack) {
        return Err(invalid("invalid terminal sequence"));
    }
    let content = encrypt_observer_payload(keys, &peer, frame)
        .map_err(|_| invalid("could not encrypt terminal frame"))?;
    EventBuilder::new(
        Kind::from(kind::KIND_SANDBOX_TERMINAL_FRAME as u16),
        content,
    )
    .tags(tags(scope.tags(peer))?)
    .sign_with_keys(keys)
    .map_err(|_| invalid("could not sign terminal frame"))
}

/// Verify sender, recipient, freshness, immutable scope and grant before decrypting bytes.
pub fn decrypt(
    keys: &Keys,
    peer: PublicKey,
    scope: Scope,
    grant: EventId,
    event: &Event,
    now: u64,
) -> Result<Frame, SdkError> {
    let created = event.created_at.as_secs();
    if event.kind.as_u16() as u32 != kind::KIND_SANDBOX_TERMINAL_FRAME
        || event.pubkey != peer
        || event.verify().is_err()
        || !scope.matches(event, keys.public_key())
        || created > now
        || now.saturating_sub(created) > MAX_FRAME_AGE_SECS
    {
        return Err(invalid("invalid terminal frame envelope"));
    }
    let frame: Frame = decrypt_observer_payload(keys, event)
        .map_err(|_| invalid("invalid terminal ciphertext"))?;
    if frame.grant != grant || (frame.seq == 0) != matches!(frame.packet, Packet::Ack) {
        return Err(invalid("terminal frame belongs to another grant"));
    }
    frame.packet.bytes()?;
    Ok(frame)
}

struct InFlight {
    frame: Frame,
    sent_ms: u64,
    retries: u8,
}

/// Bounded stop-and-retry window. A consumer must close on any returned error.
pub struct Flow {
    grant: EventId,
    sent: u64,
    received: u64,
    pending: VecDeque<InFlight>,
}

impl Flow {
    /// Create a fresh transport for one accepted owner approval.
    pub fn new(grant: EventId) -> Self {
        Self {
            grant,
            sent: 0,
            received: 0,
            pending: VecDeque::new(),
        }
    }
    /// Whether another packet fits the four-frame/64-KiB window.
    pub fn writable(&self) -> bool {
        self.pending.len() < MAX_IN_FLIGHT
    }
    /// Queue one sequenced packet; window exhaustion is explicit backpressure.
    pub fn send(&mut self, packet: Packet, now_ms: u64) -> Result<Frame, SdkError> {
        packet.bytes()?;
        if !self.writable() || matches!(packet, Packet::Ack) {
            return Err(invalid("terminal send window is full"));
        }
        self.sent = self
            .sent
            .checked_add(1)
            .ok_or_else(|| invalid("terminal sequence exhausted"))?;
        let frame = Frame {
            grant: self.grant,
            seq: self.sent,
            ack: self.received,
            packet,
        };
        self.pending.push_back(InFlight {
            frame: frame.clone(),
            sent_ms: now_ms,
            retries: 0,
        });
        Ok(frame)
    }
    /// Acknowledge contiguous traffic without causing acknowledgement loops.
    pub fn acknowledgement(&self) -> Frame {
        Frame {
            grant: self.grant,
            seq: 0,
            ack: self.received,
            packet: Packet::Ack,
        }
    }
    /// Apply peer acknowledgement and deliver each ordered packet once.
    pub fn receive(&mut self, frame: Frame) -> Result<Option<Packet>, SdkError> {
        if frame.grant != self.grant
            || frame.ack > self.sent
            || (frame.seq == 0) != matches!(frame.packet, Packet::Ack)
        {
            return Err(invalid("invalid terminal acknowledgement"));
        }
        frame.packet.bytes()?;
        while self
            .pending
            .front()
            .is_some_and(|pending| pending.frame.seq <= frame.ack)
        {
            self.pending.pop_front();
        }
        if frame.seq == 0 || frame.seq <= self.received {
            return Ok(None);
        }
        if frame.seq != self.received.saturating_add(1) {
            return Ok(None);
        }
        self.received = frame.seq;
        Ok(Some(frame.packet))
    }
    /// Retry due frames at most five times; callers re-encrypt returned frames.
    pub fn retries(&mut self, now_ms: u64) -> Result<Vec<Frame>, SdkError> {
        let mut retry = Vec::new();
        for pending in &mut self.pending {
            if now_ms.saturating_sub(pending.sent_ms) < RETRY_MS {
                continue;
            }
            if pending.retries >= MAX_RETRIES {
                return Err(invalid("terminal peer stopped acknowledging; closing"));
            }
            pending.retries += 1;
            pending.sent_ms = now_ms;
            pending.frame.ack = self.received;
            retry.push(pending.frame.clone());
        }
        Ok(retry)
    }
}

#[cfg(test)]
mod tests;
