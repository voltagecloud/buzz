//! Snapshot approvals are a distinct capability and cannot authorize a terminal.
use super::*;

/// Bounded, non-writing preflight for self-contained Git bundles.
pub mod archive;
use chacha20poly1305::{
    aead::{Aead, Payload},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use nostr::hashes::{sha256, Hash};

/// Largest ciphertext blob, matching the default generic Blossom file budget.
pub const MAX_BLOB_BYTES: usize = 100 * 1024 * 1024;
/// Source byte budget shared with the checkpoint packer.
pub const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;
/// Source-file budget shared with the checkpoint packer.
pub const MAX_FILES: usize = 50000;
const MAGIC: &[u8] = b"BUZZSNAP1\0";

/// A snapshot request, never interchangeable with a terminal pending request.
#[derive(Clone, Debug)]
pub struct Pending(super::Pending);
impl Pending {
    /// Verify the snapshot mode, signature, scope and bounded expiry.
    pub fn parse(event: &Event, now: u64) -> Result<Self, SdkError> {
        let body = read_request(event, None, "request", "snapshot", now)?;
        if body.requester != event.pubkey || body.request_id.is_some() {
            return Err(invalid("invalid snapshot requester"));
        }
        Ok(Self(super::Pending {
            event: event.clone(),
            body,
        }))
    }
    /// Signed pending request ID.
    pub fn id(&self) -> EventId {
        self.0.id()
    }
    /// Immutable sandbox scope.
    pub fn scope(&self) -> Scope {
        self.0.scope()
    }
    /// Per-session identifier.
    pub fn session(&self) -> Uuid {
        self.0.session()
    }
    /// Identity that will receive the encrypted snapshot.
    pub fn requester(&self) -> PublicKey {
        self.0.requester()
    }
    /// Latest approval expiry in Unix seconds.
    pub fn expires_at(&self) -> u64 {
        self.0.expires_at()
    }
}
/// An owner-signed snapshot capability, which cannot be passed to terminal APIs.
#[derive(Clone, Debug)]
pub struct Grant(super::Grant);
impl Grant {
    /// Verify the snapshot mode, signature, scope and bounded expiry.
    pub fn parse(event: &Event, scope: Scope, now: u64) -> Result<Self, SdkError> {
        let body = read_request(event, Some(scope), "grant", "snapshot", now)?;
        Ok(Self(super::Grant {
            event: event.clone(),
            session: body.session,
            scope,
            requester: body.requester,
            expires_at: body.expires_at,
            request_id: body
                .request_id
                .ok_or_else(|| invalid("snapshot approval has no request"))?,
        }))
    }
    /// Owner-signed approval event ID.
    pub fn id(&self) -> EventId {
        self.0.id()
    }
    /// Original metadata-only approval for durable audit.
    pub fn event(&self) -> &Event {
        self.0.event()
    }
    /// Immutable sandbox scope.
    pub fn scope(&self) -> Scope {
        self.0.scope()
    }
    /// Per-session identifier.
    pub fn session(&self) -> Uuid {
        self.0.session()
    }
    /// Approved recipient of snapshot bytes.
    pub fn requester(&self) -> PublicKey {
        self.0.requester()
    }
    /// Approval expiry in Unix seconds.
    pub fn expires_at(&self) -> u64 {
        self.0.expires_at()
    }
    /// Pending request this owner approved.
    pub fn request_id(&self) -> EventId {
        self.0.request_id()
    }
    /// Require current owner approval, live gateway authority and posting membership.
    pub fn authorized(&self, pending: &Pending, authority: Authority<'_>, now: u64) -> bool {
        self.0.authorized(&pending.0, authority, now)
    }
}
/// Build a pending snapshot request; global delegation alone never approves it.
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
            mode: "snapshot".into(),
            session,
            scope,
            expires_at,
            requester: keys.public_key(),
            request_id: None,
        },
        "request",
    )
}
/// Approve only this snapshot request, never extending its expiry.
pub fn approve(owner: &Keys, pending: &Pending, expires_at: u64) -> Result<Event, SdkError> {
    super::approve(owner, &pending.0, expires_at)
}
/// A public failure category; never contains scanner output, file names or credentials.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    /// Secret scan refused the exact artifact.
    Scan,
    /// Byte, file, object, time or capacity budget was exceeded.
    Budget,
    /// Archive creation or integrity validation failed.
    Archive,
    /// Encrypted blob upload failed.
    Upload,
    /// Session authority ended during creation.
    Authority,
}
/// Durable snapshot acceptance/end metadata; no file content or encryption key.
/// Persist one accepted event per attempt and reuse that exact event on retries.
pub fn audit(
    keys: &Keys,
    grant: &Grant,
    status: &str,
    bytes: u64,
    reason: Option<Failure>,
) -> Result<Event, SdkError> {
    if keys.public_key() != grant.scope().agent
        || !matches!(status, "accepted" | "ended")
        || (status == "accepted" && reason.is_some())
    {
        return Err(invalid("invalid snapshot audit"));
    }
    let mut values = grant.scope().tags(grant.requester());
    if status == "accepted" {
        values.push(vec!["buzz:attach-claim".into(), Uuid::new_v4().to_string()]);
    }
    values.push(vec![
        "d".into(),
        format!("snapshot:{}:{status}", grant.session()),
    ]);
    EventBuilder::new(Kind::from(kind::KIND_SANDBOX_ATTACH as u16),serde_json::json!({"version":1,"mode":"snapshot","session":grant.session(),"status":status,"grant":grant.event(),"bytes":bytes,"reason":reason}).to_string()).tags(tags(values)?).sign_with_keys(keys).map_err(|_|invalid("could not sign snapshot audit"))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u8,
    grant: EventId,
    expires_at: u64,
    sha256: String,
    size: u64,
    key: [u8; 32],
    nonce: [u8; 24],
}
/// Ciphertext and private key material. Deliberately has no Debug or Serialize implementation.
pub struct Sealed {
    ciphertext: Vec<u8>,
    metadata: Metadata,
}
impl Sealed {
    /// Exact bytes uploaded to Blossom; only ciphertext crosses this boundary.
    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }
    /// SHA-256 of those ciphertext bytes.
    pub fn hash(&self) -> &str {
        &self.metadata.sha256
    }
}
fn aad(grant: &Grant) -> Result<Vec<u8>, SdkError> {
    serde_json::to_vec(&(grant.id(), grant.scope(), grant.requester()))
        .map_err(|_| invalid("invalid snapshot scope"))
}
fn digest(bytes: &[u8]) -> String {
    sha256::Hash::hash(bytes).to_string()
}
/// Encrypt a bounded archive using a fresh random XChaCha20-Poly1305 key.
/// The key is available only through the recipient-encrypted result event.
pub fn seal(grant: &Grant, archive: &[u8]) -> Result<Sealed, SdkError> {
    if archive.len() > MAX_BLOB_BYTES - MAGIC.len() - 16 {
        return Err(invalid("snapshot exceeds 100 MiB blob budget"));
    }
    let key = Keys::generate().secret_key().secret_bytes();
    let mut nonce = [0u8; 24];
    nonce[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    nonce[16..].copy_from_slice(&Uuid::new_v4().as_bytes()[..8]);
    let cipher =
        XChaCha20Poly1305::new_from_slice(&key).map_err(|_| invalid("invalid snapshot key"))?;
    let encrypted = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: archive,
                aad: &aad(grant)?,
            },
        )
        .map_err(|_| invalid("could not encrypt snapshot"))?;
    let mut ciphertext = MAGIC.to_vec();
    ciphertext.extend(encrypted);
    let metadata = Metadata {
        version: 1,
        grant: grant.id(),
        expires_at: grant.expires_at(),
        sha256: digest(&ciphertext),
        size: ciphertext.len() as u64,
        key,
        nonce,
    };
    Ok(Sealed {
        ciphertext,
        metadata,
    })
}
/// Build the agent's recipient-encrypted result after upload confirmation.
/// The event stores no plaintext key or file bytes and carries NIP-40 expiry.
pub fn result(keys: &Keys, grant: &Grant, sealed: &Sealed) -> Result<Event, SdkError> {
    if keys.public_key() != grant.scope().agent || sealed.metadata.grant != grant.id() {
        return Err(invalid("invalid snapshot result signer or grant"));
    }
    let content = encrypt_observer_payload(keys, &grant.requester(), &sealed.metadata)
        .map_err(|_| invalid("could not encrypt snapshot receipt"))?;
    let mut values = grant.scope().tags(grant.requester());
    values.push(vec![
        "d".into(),
        format!("snapshot:{}:ready", grant.session()),
    ]);
    values.push(vec!["expiration".into(), grant.expires_at().to_string()]);
    EventBuilder::new(Kind::from(kind::KIND_SANDBOX_ATTACH as u16), content)
        .tags(tags(values)?)
        .sign_with_keys(keys)
        .map_err(|_| invalid("could not sign snapshot result"))
}
/// Verified private receipt. Callers must still refresh current grant authority before download.
pub struct Receipt {
    metadata: Metadata,
    aad: Vec<u8>,
}
impl Receipt {
    /// Ciphertext hash used to download only from the same community Blossom server.
    pub fn hash(&self) -> &str {
        &self.metadata.sha256
    }
    /// Exact ciphertext byte count; enforce this while streaming the download.
    pub fn size(&self) -> u64 {
        self.metadata.size
    }
    /// Verify ciphertext hash/length, then authenticate and decrypt the archive.
    pub fn open(&self, ciphertext: &[u8], now: u64) -> Result<Vec<u8>, SdkError> {
        if now >= self.metadata.expires_at
            || ciphertext.len() as u64 != self.metadata.size
            || ciphertext.len() > MAX_BLOB_BYTES
            || !ciphertext.starts_with(MAGIC)
            || digest(ciphertext) != self.metadata.sha256
        {
            return Err(invalid("expired or mismatched snapshot blob"));
        }
        let cipher = XChaCha20Poly1305::new_from_slice(&self.metadata.key)
            .map_err(|_| invalid("invalid snapshot key"))?;
        cipher
            .decrypt(
                XNonce::from_slice(&self.metadata.nonce),
                Payload {
                    msg: &ciphertext[MAGIC.len()..],
                    aad: &self.aad,
                },
            )
            .map_err(|_| invalid("snapshot authentication failed"))
    }
}
/// Verify the signed agent result, immutable scope, approval, recipient and expiry.
pub fn receipt(keys: &Keys, grant: &Grant, event: &Event, now: u64) -> Result<Receipt, SdkError> {
    if keys.public_key() != grant.requester()
        || event.pubkey != grant.scope().agent
        || event.kind.as_u16() as u32 != kind::KIND_SANDBOX_ATTACH
        || event.verify().is_err()
        || event.content.len() > 4096
        || event.created_at.as_secs() > now
        || now >= grant.expires_at()
        || !grant.scope().matches(event, grant.requester())
        || tag(event, "d") != Some(format!("snapshot:{}:ready", grant.session()).as_str())
        || tag(event, "expiration").and_then(|s| s.parse::<u64>().ok()) != Some(grant.expires_at())
    {
        return Err(invalid("invalid snapshot receipt envelope"));
    }
    let metadata: Metadata = decrypt_observer_payload(keys, event)
        .map_err(|_| invalid("invalid encrypted snapshot receipt"))?;
    if metadata.version != 1
        || metadata.grant != grant.id()
        || metadata.expires_at != grant.expires_at()
        || metadata.size > MAX_BLOB_BYTES as u64
        || metadata.size < (MAGIC.len() + 16) as u64
        || metadata.sha256.len() != 64
        || !metadata
            .sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(invalid("invalid snapshot receipt metadata"));
    }
    Ok(Receipt {
        metadata,
        aad: aad(grant)?,
    })
}
#[cfg(test)]
mod tests;
