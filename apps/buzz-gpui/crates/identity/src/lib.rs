//! The human's Buzz identity.
//!
//! The key is in the OS keyring entry buzz-app used before it moved its keys
//! into one combined entry; [`adopt_buzz_app`] copies the key from that
//! entry, so both apps share one identity. Nothing outside this crate sees
//! the secret key. Callers
//! ask for a specific purpose (relay auth, an HTTP request, a chat event)
//! rather than "sign this", so a bug elsewhere cannot mint HTTP or media
//! tokens or forge kinds the app never writes.

use base64::Engine;
use buzz_keychain::Error as StoreError;
use nostr::nips::nip19::ToBech32;
use nostr::nips::nip44;
use nostr::prelude::{
    Event, EventBuilder, FinalizeEvent, Keys, Kind, PublicKey, SecretKey, SignEvent, Tag,
    Timestamp, UnsignedEvent,
};
use sha2::{Digest, Sha256};
use std::fmt;
use zeroize::Zeroizing;

mod reminders;
#[cfg(feature = "session-control")]
mod session_control;
pub use reminders::{DecodedReminder, ReminderIntent};

/// buzz-app's release keyring entry. Debug builds of buzz-app use a
/// `.debug` service; this app always uses the release one so it shares the
/// identity the user actually signs in with.
pub const SERVICE: &str = "dev.local.buzz.foundation.identity";
pub const ACCOUNT: &str = "human";

/// Largest event this app signs, as serialized JSON.
pub const MAX_EVENT_BYTES: usize = 64 * 1024;

/// Storage messages are buzz-app's (`src-tauri/src/identity.rs`), so both
/// apps explain the shared keyring the same way.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("Secure storage access was denied. Allow access and retry; no identity was created.")]
    Denied,
    #[error("Another Buzz app is accessing secure storage. Retry shortly.")]
    Busy,
    #[error(
        "Your identity could not be accessed in secure storage. Unlock your credential store \
         and retry without changing keys."
    )]
    Unavailable,
    #[error(
        "Saved identity is malformed; nothing was changed. Keep your key backup and contact \
         support before changing secure storage."
    )]
    Malformed,
    /// A key was found before saving, so nothing was generated or written.
    #[error("An identity is already saved; nothing was overwritten")]
    AlreadySaved,
    /// Another process saved a key between the read and the write.
    #[error("An identity is already saved; nothing was overwritten. Restart to restore it.")]
    Occupied,
    /// The key was deleted, but a fresh read still finds it.
    #[error("Your key is still saved in secure storage")]
    StillSaved,
    #[error("Enter a valid nsec private key")]
    InvalidKey,
    #[error("This app does not sign that kind of event.")]
    NotAllowed,
    #[error("The event is too large to sign.")]
    TooLarge,
    #[error("Could not sign: {0}")]
    Signing(String),
    #[error("Could not decrypt.")]
    Decrypt,
}

fn store_error(error: StoreError) -> IdentityError {
    match error {
        StoreError::Denied => IdentityError::Denied,
        StoreError::Busy => IdentityError::Busy,
        StoreError::Corrupt => IdentityError::Malformed,
        StoreError::Occupied => IdentityError::Occupied,
        StoreError::Absent | StoreError::Unavailable => IdentityError::Unavailable,
    }
}

pub struct Identity {
    keys: Keys,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("pubkey", &self.keys.public_key().to_hex())
            .finish_non_exhaustive()
    }
}

/// Reads the saved identity. `Ok(None)` means none is saved; errors never
/// lead to generating a key.
pub fn load() -> Result<Option<Identity>, IdentityError> {
    match buzz_keychain::read(SERVICE, ACCOUNT) {
        Ok(bytes) => {
            let text = std::str::from_utf8(&bytes).map_err(|_| IdentityError::Malformed)?;
            Identity::from_nsec(text)
                .map(Some)
                .map_err(|_| IdentityError::Malformed)
        }
        Err(StoreError::Absent) => Ok(None),
        Err(error) => Err(store_error(error)),
    }
}

/// Saves a new random identity. Refuses if one is already saved.
pub fn create() -> Result<Identity, IdentityError> {
    ensure_absent()?;
    save(Identity {
        keys: Keys::generate(),
    })
}

/// Saves an imported `nsec`. Refuses if one is already saved.
pub fn import(nsec: &str) -> Result<Identity, IdentityError> {
    ensure_absent()?;
    save(Identity::from_nsec(nsec)?)
}

/// buzz-app restores before it saves: a saved key, or one that can't be
/// read, stops the save before a key is generated or written.
fn ensure_absent() -> Result<(), IdentityError> {
    match load()? {
        Some(_) => Err(IdentityError::AlreadySaved),
        None => Ok(()),
    }
}

/// Deletes the saved identity (sign out), then confirms a fresh read finds
/// nothing. buzz-app shares this entry, so it is signed out too.
pub fn forget() -> Result<(), IdentityError> {
    match buzz_keychain::delete(SERVICE, ACCOUNT) {
        Ok(()) | Err(StoreError::Absent) => {}
        Err(error) => return Err(store_error(error)),
    }
    match buzz_keychain::read(SERVICE, ACCOUNT) {
        Err(StoreError::Absent) => Ok(()),
        Ok(_) => Err(IdentityError::StillSaved),
        Err(error) => Err(store_error(error)),
    }
}

/// buzz-app's store since it moved its keys out of [`SERVICE`]: one entry
/// whose value is a JSON map of every key it holds. The human key is under
/// [`BUZZ_APP_IDENTITY`].
pub const BUZZ_APP_SERVICE: &str = "buzz-desktop";
const BUZZ_APP_ACCOUNT: &str = "secrets";
const BUZZ_APP_IDENTITY: &str = "identity";

/// Copies buzz-app's identity into this app's entry. `Ok(None)` means
/// buzz-app keeps no identity. buzz-app's entry is only read, so both apps
/// keep working with the same key.
pub fn adopt_buzz_app() -> Result<Option<Identity>, IdentityError> {
    let bytes = match buzz_keychain::read(BUZZ_APP_SERVICE, BUZZ_APP_ACCOUNT) {
        Ok(bytes) => bytes,
        Err(StoreError::Absent) => return Ok(None),
        Err(error) => return Err(store_error(error)),
    };
    let Some(identity) = buzz_app_identity(&bytes)? else {
        return Ok(None);
    };
    match save(identity) {
        // Another process saved a key first; that one stands.
        Err(IdentityError::Occupied) => load(),
        saved => saved.map(Some),
    }
}

/// The human key in buzz-app's map. The other keys are its agents'; they
/// are wiped, not kept.
fn buzz_app_identity(bytes: &[u8]) -> Result<Option<Identity>, IdentityError> {
    let mut secrets: std::collections::HashMap<String, String> =
        serde_json::from_slice(bytes).map_err(|_| IdentityError::Malformed)?;
    let nsec = secrets.remove(BUZZ_APP_IDENTITY).map(Zeroizing::new);
    for value in secrets.values_mut() {
        zeroize::Zeroize::zeroize(value);
    }
    nsec.map(|nsec| Identity::from_nsec(&nsec).map_err(|_| IdentityError::Malformed))
        .transpose()
}

fn save(identity: Identity) -> Result<Identity, IdentityError> {
    let nsec = identity.nsec();
    buzz_keychain::add(SERVICE, ACCOUNT, nsec.as_bytes()).map_err(store_error)?;
    // Commit in memory only once storage confirms.
    Ok(identity)
}

/// What a signed event is for. Each purpose fixes the kinds it may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// Chat, reactions, edits, deletions, reports, profile, status, emoji,
    /// channel membership, community requests and Canvas saves.
    Content,
    /// NIP-44 self-encrypted app data (read state, sidebar preferences).
    AppData,
    /// Presence over the live socket.
    Presence,
}

/// NIP-PS plugin release signatures.
pub const PLUGIN_RELEASE: u16 = 1064;

/// Kinds the app writes as ordinary content.
const CONTENT_KINDS: &[u16] = &[
    0, 5, 7, 9, 1984, 9000, 9001, 9002, 9007, 9008, 9021, 9022, 9030, 9031, 9032, 9035, 9036,
    28936, 30030, 30175, 30177, 30178, 30315, 30620, 40003, 40100, 41010, 41012, 42000, 45010,
    46020,
];

impl Identity {
    /// Parses an `nsec1…` string (hex is not accepted, as in buzz-app).
    pub fn from_nsec(text: &str) -> Result<Self, IdentityError> {
        let text = text.trim();
        if text.len() != 63 || !text.to_ascii_lowercase().starts_with("nsec1") {
            return Err(IdentityError::InvalidKey);
        }
        let secret =
            SecretKey::parse(&text.to_ascii_lowercase()).map_err(|_| IdentityError::InvalidKey)?;
        Ok(Self {
            keys: Keys::new(secret),
        })
    }

    /// A throwaway identity that is never saved.
    pub fn ephemeral() -> Self {
        Self {
            keys: Keys::generate(),
        }
    }

    pub fn public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    pub fn pubkey_hex(&self) -> String {
        self.keys.public_key().to_hex()
    }

    /// For explicit exports only: the "export key" action, and Pair
    /// mobile's end-to-end encrypted transfer to the user's phone.
    pub fn nsec(&self) -> Zeroizing<String> {
        Zeroizing::new(
            self.keys
                .secret_key()
                .to_bech32()
                .expect("secret keys encode as nsec"),
        )
    }

    /// Builds the unsigned event, with its final ID, that [`Self::sign`]
    /// will sign. The outbox saves this before signing.
    pub fn template(
        &self,
        purpose: Purpose,
        kind: u16,
        created_at: u64,
        tags: Vec<Vec<String>>,
        content: String,
    ) -> Result<UnsignedEvent, IdentityError> {
        check_kind(purpose, kind)?;
        if kind == AGENT_PRESENCE {
            check_agent_presence(&tags, &content)?;
        }
        let tags = tags
            .into_iter()
            .map(|tag| Tag::parse(tag).map_err(|error| IdentityError::Signing(error.to_string())))
            .collect::<Result<Vec<_>, _>>()?;
        let mut unsigned = UnsignedEvent::new(
            self.keys.public_key(),
            Timestamp::from_secs(created_at),
            Kind::from_u16(kind),
            tags,
            content,
        );
        unsigned.ensure_id();
        if unsigned.as_json().len() > MAX_EVENT_BYTES {
            return Err(IdentityError::TooLarge);
        }
        Ok(unsigned)
    }

    /// Signs an event built by [`Self::template`], keeping its ID.
    pub fn sign(&self, purpose: Purpose, unsigned: UnsignedEvent) -> Result<Event, IdentityError> {
        check_kind(purpose, unsigned.kind.as_u16())?;
        if unsigned.kind.as_u16() == AGENT_PRESENCE {
            let tags: Vec<Vec<String>> = unsigned
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect();
            check_agent_presence(&tags, &unsigned.content)?;
        }
        if unsigned.pubkey != self.keys.public_key() {
            return Err(IdentityError::NotAllowed);
        }
        self.keys
            .sign_event(unsigned)
            .map_err(|error| IdentityError::Signing(error.to_string()))
    }

    /// NIP-42 relay authentication: kind 22242 with exactly the relay URL
    /// (`wss://host`) and the challenge.
    pub fn relay_auth(&self, relay: &str, challenge: &str) -> Result<Event, IdentityError> {
        if challenge.is_empty() || challenge.len() > 4096 || !relay.starts_with("ws") {
            return Err(IdentityError::NotAllowed);
        }
        EventBuilder::new(Kind::Authentication, "")
            .tags([
                Tag::custom("relay", [relay]),
                Tag::custom("challenge", [challenge]),
            ])
            .finalize(&self.keys)
            .map_err(|error| IdentityError::Signing(error.to_string()))
    }

    /// NIP-98 `Authorization` header value for one request. A fresh event
    /// (with a random nonce) every time: the relay refuses reused ones.
    pub fn http_auth(
        &self,
        url: &str,
        method: &str,
        body: Option<&[u8]>,
    ) -> Result<String, IdentityError> {
        let mut tags = vec![Tag::custom("u", [url]), Tag::custom("method", [method])];
        if let Some(body) = body {
            tags.push(Tag::custom(
                "payload",
                [format!("{:x}", Sha256::digest(body))],
            ));
        }
        tags.push(Tag::custom("nonce", [uuid::Uuid::new_v4().to_string()]));
        let event = EventBuilder::new(Kind::HttpAuth, "")
            .tags(tags)
            .finalize(&self.keys)
            .map_err(|error| IdentityError::Signing(error.to_string()))?;
        Ok(header(&event))
    }

    /// NIP-98 for one Buzz Git repository (`relay_git_authorization`):
    /// kind 27235 with exactly `u` and `method GET`, no nonce or payload,
    /// which the relay accepts for 60 seconds on every Git route of that
    /// repository. `repository` must be the canonical
    /// `https://<host>/git/<owner hex>/<name>` URL; the caller binds it to
    /// its community's origin. Never a general signing capability.
    pub fn git_auth(&self, repository: &str) -> Result<Event, IdentityError> {
        if !git_repository_url(repository) {
            return Err(IdentityError::NotAllowed);
        }
        EventBuilder::new(Kind::HttpAuth, "")
            .tags([
                Tag::custom("u", [repository]),
                Tag::custom("method", ["GET"]),
            ])
            .finalize(&self.keys)
            .map_err(|error| IdentityError::Signing(error.to_string()))
    }

    /// A plugin release signature (buzz-app's NIP-PS, kind 1064): empty
    /// content and exactly an `x` tag with the SHA-256 of the release
    /// file and a `t` tag naming the release format. It vouches for one
    /// exact file and nothing else.
    pub fn sign_plugin_release(&self, sha256: &str, format: &str) -> Result<Event, IdentityError> {
        let hex = sha256.len() == 64
            && sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !hex || !format.starts_with("buzz-") || format.len() > 64 {
            return Err(IdentityError::NotAllowed);
        }
        EventBuilder::new(Kind::from_u16(PLUGIN_RELEASE), "")
            .tags([Tag::custom("x", [sha256]), Tag::custom("t", [format])])
            .finalize(&self.keys)
            .map_err(|error| IdentityError::Signing(error.to_string()))
    }

    /// Blossom (kind 24242) authorization for one media request, valid for
    /// 60 seconds. `server` is the relay's `host[:port]`.
    pub fn media_auth(
        &self,
        verb: MediaVerb,
        server: &str,
        sha256: Option<&str>,
    ) -> Result<String, IdentityError> {
        let now = Timestamp::now().as_secs();
        let (content, verb) = match verb {
            MediaVerb::Upload => ("Upload attachment", "upload"),
            MediaVerb::Get => ("Get buzz-media", "get"),
        };
        let mut tags = Vec::new();
        if let Some(hash) = sha256 {
            tags.push(Tag::custom("x", [hash]));
        }
        tags.push(Tag::custom("t", [verb]));
        tags.push(Tag::custom("server", [server]));
        tags.push(Tag::custom("expiration", [(now + 60).to_string()]));
        let event = EventBuilder::new(Kind::from_u16(24242), content)
            .tags(tags)
            .finalize(&self.keys)
            .map_err(|error| IdentityError::Signing(error.to_string()))?;
        Ok(header(&event))
    }

    /// Signs a Blossom media authorization that buzz-desktop's upload and
    /// media services built, after checking it is exactly what
    /// [`Self::media_auth`] would sign: kind 24242 with the fixed content of
    /// its verb, an `x` hash only for an upload, one `server`, and an
    /// expiry at most 60 seconds after a current `created_at`. Anything
    /// else is refused, so those services cannot mint other tokens.
    pub fn sign_media_event(
        &self,
        kind: u16,
        created_at: u64,
        tags: &[Vec<String>],
        content: &str,
    ) -> Result<Event, IdentityError> {
        let now = Timestamp::now().as_secs();
        let tag = |name: &str| {
            let mut found = tags
                .iter()
                .filter(|tag| tag.first().map(String::as_str) == Some(name));
            match (found.next(), found.next()) {
                (Some(tag), None) if tag.len() == 2 => Some(tag[1].as_str()),
                _ => None,
            }
        };
        let verb = tag("t");
        let hash = tag("x");
        let hex = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        let shape_ok = match (verb, content) {
            (Some("upload"), "Upload attachment") => hash.is_some_and(hex) && tags.len() == 4,
            (Some("get"), "Get buzz-media") => hash.is_none() && tags.len() == 3,
            _ => false,
        };
        let server_ok = tag("server").is_some_and(|server| {
            !server.is_empty()
                && server.len() <= 255
                && server
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
        });
        let expiry_ok = tag("expiration")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|expiry| expiry > created_at && expiry <= created_at + 60);
        if kind != 24242 || !shape_ok || !server_ok || !expiry_ok || created_at.abs_diff(now) > 300
        {
            return Err(IdentityError::NotAllowed);
        }
        let tags = tags
            .iter()
            .map(|tag| {
                Tag::parse(tag.clone()).map_err(|error| IdentityError::Signing(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        EventBuilder::new(Kind::from_u16(kind), content)
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(created_at))
            .finalize(&self.keys)
            .map_err(|error| IdentityError::Signing(error.to_string()))
    }

    /// NIP-OA owner attestation for an agent key, as the JSON `auth` tag
    /// the agent host stores: `["auth", owner, "", signature]`.
    pub fn attest_agent(&self, agent_pubkey: &str) -> Result<String, IdentityError> {
        if agent_pubkey.len() != 64
            || !agent_pubkey
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            || agent_pubkey == self.pubkey_hex()
        {
            return Err(IdentityError::NotAllowed);
        }
        let digest: [u8; 32] = Sha256::digest(format!("nostr:agent-auth:{agent_pubkey}:")).into();
        let signature = self.keys.sign_schnorr(digest);
        Ok(
            serde_json::to_string(&["auth", &self.pubkey_hex(), "", &signature.to_string()])
                .expect("strings serialize"),
        )
    }

    /// The owner's signed intent to set up `agent_pubkey` in the community
    /// whose relay is `relay_url` (`wss://host`), as buzz-app's Use here
    /// sends it to the agent host (`relay_agent_resolve`).
    pub fn resolve_agent_community(
        &self,
        agent_pubkey: &str,
        relay_url: &str,
    ) -> Result<String, IdentityError> {
        if !relay_url.starts_with("wss://") || relay_url.ends_with('/') {
            return Err(IdentityError::NotAllowed);
        }
        let viewer = self.pubkey_hex();
        let target = buzz_desktop::relay::agent::AgentTarget {
            pubkey: agent_pubkey.to_owned(),
            owner: viewer.clone(),
            confirmed: Some(true),
        };
        let proof = buzz_desktop::relay::agent::resolve(
            &self.secret_bytes(),
            &viewer,
            relay_url.to_owned(),
            target,
        )
        .map_err(|_| IdentityError::NotAllowed)?;
        proof["signature"]
            .as_str()
            .map(str::to_owned)
            .ok_or(IdentityError::NotAllowed)
    }

    /// The owner's signature authorizing one read of an agent's harness log
    /// (`relay_agent_log_proof`): `id` is the agent's log session, `nonce`
    /// the host's challenge, and `relay_url` must be the community's relay.
    pub fn harness_log_proof(
        &self,
        community: &str,
        id: &str,
        agent_pubkey: &str,
        relay_url: &str,
        nonce: &str,
    ) -> Result<String, String> {
        use buzz_desktop::relay::agent::{LogTarget, log_proof, log_relays};
        let target = LogTarget {
            id: id.to_owned(),
            pubkey: agent_pubkey.to_owned(),
            relay_url: relay_url.to_owned(),
            nonce: nonce.to_owned(),
        };
        let relays = log_relays(community, &target)?;
        log_proof(&self.secret_bytes(), &self.pubkey_hex(), relays, target)
    }

    /// NIP-44 v2 encryption to our own key, for app data only.
    pub fn encrypt_to_self(&self, plaintext: &str) -> Result<String, IdentityError> {
        nip44::encrypt(
            self.keys.secret_key(),
            &self.keys.public_key(),
            plaintext,
            nip44::Version::V2,
        )
        .map_err(|error| IdentityError::Signing(error.to_string()))
    }

    /// Decrypts app data this identity encrypted to itself. The event must
    /// be ours and already verified by the caller.
    pub fn decrypt_from_self(&self, event: &Event) -> Result<String, IdentityError> {
        if event.pubkey != self.keys.public_key() {
            return Err(IdentityError::NotAllowed);
        }
        nip44::decrypt(
            self.keys.secret_key(),
            &self.keys.public_key(),
            &event.content,
        )
        .map_err(|_| IdentityError::Decrypt)
    }

    /// Decrypts an agent's activity frame or turn metrics (kinds 24200 and
    /// 44200), which agents encrypt to their owner. Nothing else is opened
    /// here; the event must be verified by the caller.
    pub fn decrypt_agent_frame(&self, event: &Event) -> Result<String, IdentityError> {
        let ours = self.keys.public_key().to_hex();
        let addressed = event.tags.iter().any(|tag| {
            let tag = tag.as_slice();
            tag.first().map(String::as_str) == Some("p") && tag.get(1) == Some(&ours)
        });
        if !matches!(event.kind.as_u16(), 24200 | 44200) || !addressed {
            return Err(IdentityError::NotAllowed);
        }
        nip44::decrypt(self.keys.secret_key(), &event.pubkey, &event.content)
            .map_err(|_| IdentityError::Decrypt)
    }
}

/// Owner-key decoding of agent records with buzz-app's native decoders
/// (`buzz_desktop::relay::agent`). Each opens only what agents address to
/// this identity; the secret never leaves this crate.
impl Identity {
    fn secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.keys.secret_key().secret_bytes())
    }

    /// The newest memory per slug an agent wrote to this owner (kind 30174),
    /// from the records of a `memory_query` read: `{entries, partial}`.
    pub fn decode_agent_memories(
        &self,
        agent: &str,
        records: &[Option<serde_json::Value>],
    ) -> Result<serde_json::Value, String> {
        let viewer = self.pubkey_hex();
        buzz_desktop::relay::agent::decode_memories(
            &self.secret_bytes(),
            &viewer,
            &viewer,
            agent,
            records,
        )
    }

    /// The plaintext of an agent's observer frame (kind 24200) or saved
    /// frame (44200). `history` accepts frames up to 90 days old instead of
    /// five minutes: `{id, agent, createdAt, plaintext}`.
    pub fn decode_agent_frame(
        &self,
        event: &buzz_desktop::relay::agent::AgentEvent,
        history: bool,
    ) -> Result<serde_json::Value, String> {
        buzz_desktop::relay::agent::decode_archive(
            &self.secret_bytes(),
            &self.pubkey_hex(),
            event,
            history,
        )
    }

    /// Decrypts a page read from the local activity archive.
    pub fn decode_archive_page(
        &self,
        page: buzz_desktop::archive::Page,
    ) -> Result<serde_json::Value, String> {
        page.decode(&self.secret_bytes(), &self.pubkey_hex())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MediaVerb {
    Upload,
    Get,
}

fn check_kind(purpose: Purpose, kind: u16) -> Result<(), IdentityError> {
    let allowed = match purpose {
        Purpose::Content => CONTENT_KINDS.contains(&kind),
        Purpose::AppData => kind == 30078,
        // The viewer's own presence, and its agents' (owner presence).
        Purpose::Presence => kind == 20001 || kind == AGENT_PRESENCE,
    };
    if allowed {
        Ok(())
    } else {
        Err(IdentityError::NotAllowed)
    }
}

/// Owner presence for the viewer's agents.
pub const AGENT_PRESENCE: u16 = 20003;

/// buzz-app's native check of owner presence: `online` or `offline`, and
/// one to 256 `p` tags of agent keys; the relay keeps only the agents the
/// signer owns.
fn check_agent_presence(tags: &[Vec<String>], content: &str) -> Result<(), IdentityError> {
    let hex_key =
        |key: &str| key.len() == 64 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !matches!(content, "online" | "offline")
        || tags.is_empty()
        || tags.len() > 256
        || !tags
            .iter()
            .all(|tag| matches!(tag.as_slice(), [name, key] if name == "p" && hex_key(key)))
    {
        return Err(IdentityError::NotAllowed);
    }
    Ok(())
}
/// A canonical Buzz Git repository URL, `https://<host>/git/<owner>/<name>`
/// (`git_repository` in buzz-app's `relay.rs`): lowercase hex owner, and a
/// name of 1-64 of `[A-Za-z0-9._-]` after one optional `.git`, with no
/// leading dot and no `..`.
fn git_repository_url(repository: &str) -> bool {
    let Ok(url) = url::Url::parse(repository) else {
        return false;
    };
    let segments: Vec<&str> = url.path().split('/').skip(1).collect();
    let name = |value: &str| {
        let value = value.strip_suffix(".git").unwrap_or(value);
        !value.is_empty()
            && value.len() <= 64
            && !value.starts_with('.')
            && !value.contains("..")
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    url.as_str() == repository
        && url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && segments.len() == 3
        && segments[0] == "git"
        && segments[1].len() == 64
        && segments[1]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && name(segments[2])
}

fn header(event: &Event) -> String {
    let json = serde_json::to_vec(event).expect("events serialize");
    format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(json)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// buzz-app's map gives up its human key and nothing else.
    #[test]
    fn buzz_app_identity_is_read_from_its_secrets_map() {
        let human = Identity::ephemeral();
        let agent = Identity::ephemeral();
        let map = serde_json::json!({
            "identity": *human.nsec(),
            "agent.0123": *agent.nsec(),
        });
        let found = buzz_app_identity(map.to_string().as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(found.pubkey_hex(), human.pubkey_hex());

        let agents_only = serde_json::json!({ "agent.0123": *agent.nsec() });
        assert!(
            buzz_app_identity(agents_only.to_string().as_bytes())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            buzz_app_identity(b"not json").unwrap_err(),
            IdentityError::Malformed
        );
        assert_eq!(
            buzz_app_identity(br#"{"identity":"nsec1bad"}"#).unwrap_err(),
            IdentityError::Malformed
        );
    }

    /// An observer frame an agent encrypted to its owner opens only for
    /// that owner, through buzz-desktop's decoder.
    #[test]
    fn agent_frames_open_only_for_their_owner() {
        let owner = Identity::ephemeral();
        let agent = Keys::generate();
        let plaintext = r#"{"type":"status","message":"active"}"#;
        let content = nip44::encrypt(
            agent.secret_key(),
            &owner.public_key(),
            plaintext,
            nip44::Version::V2,
        )
        .unwrap();
        let event = EventBuilder::new(Kind::from_u16(24200), content)
            .tags([
                Tag::custom("p", [owner.pubkey_hex()]),
                Tag::custom("agent", [agent.public_key().to_hex()]),
                Tag::custom("frame", ["telemetry"]),
            ])
            .finalize(&agent)
            .unwrap();
        let event: buzz_desktop::relay::agent::AgentEvent =
            serde_json::from_str(&event.as_json()).unwrap();
        let frame = owner.decode_agent_frame(&event, false).unwrap();
        assert_eq!(frame["plaintext"], plaintext);
        assert_eq!(frame["agent"], agent.public_key().to_hex());
        assert!(
            Identity::ephemeral()
                .decode_agent_frame(&event, false)
                .is_err()
        );
        // Memories need a real agent target.
        assert!(
            owner
                .decode_agent_memories(&owner.pubkey_hex(), &[])
                .is_err()
        );
        let empty = owner
            .decode_agent_memories(&agent.public_key().to_hex(), &[])
            .unwrap();
        assert_eq!(empty["partial"], false);
    }

    #[test]
    fn nsec_round_trips_and_hex_is_refused() {
        let identity = Identity::ephemeral();
        let nsec = identity.nsec();
        let parsed = Identity::from_nsec(&nsec).unwrap();
        assert_eq!(parsed.public_key(), identity.public_key());
        assert_eq!(
            Identity::from_nsec(&identity.keys.secret_key().to_secret_hex()).unwrap_err(),
            IdentityError::InvalidKey
        );
    }

    #[test]
    fn template_id_is_kept_when_signing() {
        let identity = Identity::ephemeral();
        let unsigned = identity
            .template(
                Purpose::Content,
                9,
                1_700_000_000,
                vec![vec!["h".into(), "general".into()]],
                "hi".into(),
            )
            .unwrap();
        let id = unsigned.id.unwrap();
        let event = identity.sign(Purpose::Content, unsigned).unwrap();
        assert_eq!(event.id, id);
        assert!(event.verify().is_ok());
    }

    #[test]
    fn signs_owner_presence_only_in_its_shape() {
        let identity = Identity::ephemeral();
        let agent = "a".repeat(64);
        let presence = |content: &str, tags: Vec<Vec<String>>| {
            identity.template(Purpose::Presence, AGENT_PRESENCE, 1, tags, content.into())
        };
        let unsigned = presence("online", vec![vec!["p".into(), agent.clone()]]).unwrap();
        assert!(identity.sign(Purpose::Presence, unsigned).is_ok());
        for (content, tags) in [
            ("away", vec![vec!["p".to_string(), agent.clone()]]),
            ("online", vec![]),
            ("online", vec![vec!["p".into(), "npub1x".into()]]),
            ("offline", vec![vec!["e".into(), agent.clone()]]),
            ("offline", vec![vec!["p".into(), agent.clone()]; 257]),
        ] {
            assert_eq!(
                presence(content, tags).unwrap_err(),
                IdentityError::NotAllowed
            );
        }
        // Only as presence.
        assert_eq!(
            identity
                .template(
                    Purpose::Content,
                    AGENT_PRESENCE,
                    1,
                    Vec::new(),
                    "online".into()
                )
                .unwrap_err(),
            IdentityError::NotAllowed
        );
    }

    #[test]
    fn purposes_limit_kinds() {
        let identity = Identity::ephemeral();
        for (purpose, kind) in [
            (Purpose::Content, 27235),
            (Purpose::Content, 24242),
            (Purpose::Content, 30078),
            (Purpose::AppData, 9),
            (Purpose::Presence, 9),
        ] {
            assert_eq!(
                identity
                    .template(purpose, kind, 1, Vec::new(), String::new())
                    .unwrap_err(),
                IdentityError::NotAllowed,
                "{purpose:?} {kind}"
            );
        }
    }

    #[test]
    fn agent_attestation_matches_nip_oa() {
        let identity = Identity::ephemeral();
        let agent = Keys::generate().public_key().to_hex();
        let tag: Vec<String> =
            serde_json::from_str(&identity.attest_agent(&agent).unwrap()).unwrap();
        assert_eq!(tag[0], "auth");
        assert_eq!(tag[1], identity.pubkey_hex());
        assert_eq!(tag[2], "");
        let digest: [u8; 32] = Sha256::digest(format!("nostr:agent-auth:{agent}:")).into();
        let signature: secp256k1::schnorr::Signature = tag[3].parse().unwrap();
        let owner: secp256k1::XOnlyPublicKey = tag[1].parse().unwrap();
        secp256k1::Secp256k1::verification_only()
            .verify_schnorr(&signature, &digest, &owner)
            .unwrap();
        assert!(identity.attest_agent(&identity.pubkey_hex()).is_err());
    }

    fn verify(owner: &str, text: &str, signature: &str) {
        let digest: [u8; 32] = Sha256::digest(text).into();
        let signature: secp256k1::schnorr::Signature = signature.parse().unwrap();
        let owner: secp256k1::XOnlyPublicKey = owner.parse().unwrap();
        secp256k1::Secp256k1::verification_only()
            .verify_schnorr(&signature, &digest, &owner)
            .unwrap();
    }

    #[test]
    fn owner_proofs_sign_buzz_app_texts() {
        let identity = Identity::ephemeral();
        let owner = identity.pubkey_hex();
        let agent = Keys::generate().public_key().to_hex();
        let relay = "wss://relay.example";
        let resolved = identity.resolve_agent_community(&agent, relay).unwrap();
        verify(
            &owner,
            &format!("nostr:agent-community:{agent}:{relay}"),
            &resolved,
        );
        assert!(identity.resolve_agent_community(&owner, relay).is_err());
        assert!(
            identity
                .resolve_agent_community(&agent, "wss://relay.example/")
                .is_err()
        );

        let id = format!("{agent}-{:x}", Sha256::digest(relay.as_bytes()));
        let nonce = "0e9d217a-2d7e-4d5e-bbbd-eec9257acfba";
        let proof = identity
            .harness_log_proof("https://relay.example", &id, &agent, relay, nonce)
            .unwrap();
        verify(
            &owner,
            &format!("buzz-app:harness-log:v1:{id}:{agent}:{relay}:{nonce}"),
            &proof,
        );
        let elsewhere = "wss://other.example";
        assert!(
            identity
                .harness_log_proof("https://relay.example", &id, &agent, elsewhere, nonce)
                .is_err()
        );
        assert!(
            identity
                .harness_log_proof("https://relay.example", &id, &agent, relay, "NONCE")
                .is_err()
        );
    }

    #[test]
    fn self_encryption_round_trips() {
        let identity = Identity::ephemeral();
        let ciphertext = identity.encrypt_to_self("{\"v\":1}").unwrap();
        let unsigned = identity
            .template(
                Purpose::AppData,
                30078,
                Timestamp::now().as_secs(),
                Vec::new(),
                ciphertext,
            )
            .unwrap();
        let event = identity.sign(Purpose::AppData, unsigned).unwrap();
        assert_eq!(identity.decrypt_from_self(&event).unwrap(), "{\"v\":1}");
    }

    #[test]
    fn plugin_releases_sign_one_digest() {
        let identity = Identity::ephemeral();
        let digest = "ab".repeat(32);
        let event = identity
            .sign_plugin_release(&digest, "buzz-plugin-folder-v1")
            .unwrap();
        event.verify().unwrap();
        assert_eq!(event.kind.as_u16(), PLUGIN_RELEASE);
        assert!(event.content.is_empty());
        let tags: Vec<Vec<String>> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect();
        assert_eq!(
            tags,
            [
                vec!["x".to_owned(), digest.clone()],
                vec!["t".to_owned(), "buzz-plugin-folder-v1".to_owned()]
            ]
        );
        for (digest, format) in [
            ("AB".repeat(32), "buzz-plugin-folder-v1"),
            ("ab".repeat(31), "buzz-plugin-folder-v1"),
            ("ab".repeat(32), "other"),
        ] {
            assert!(identity.sign_plugin_release(&digest, format).is_err());
        }
    }

    #[test]
    fn git_auth_signs_only_repository_urls() {
        let identity = Identity::ephemeral();
        let owner = "a".repeat(64);
        let url = format!("https://relay.example.com/git/{owner}/demo.git");
        let event = identity.git_auth(&url).unwrap();
        assert_eq!(event.kind, Kind::HttpAuth);
        assert!(event.content.is_empty());
        let tags: Vec<Vec<String>> = event.tags.iter().map(|t| t.clone().to_vec()).collect();
        assert_eq!(
            tags,
            [
                vec!["u".to_string(), url],
                vec!["method".into(), "GET".into()]
            ]
        );
        for bad in [
            format!("http://relay.example.com/git/{owner}/demo"),
            format!("https://relay.example.com/git/{owner}/demo?x=1"),
            format!("https://relay.example.com/git/{owner}/demo#x"),
            format!("https://me@relay.example.com/git/{owner}/demo"),
            format!("https://relay.example.com/git/{owner}/.demo"),
            format!("https://relay.example.com/git/{owner}/a..b"),
            format!("https://relay.example.com/git/{owner}/demo/info/refs"),
            format!("https://relay.example.com/git/{}/demo", "A".repeat(64)),
            "https://relay.example.com/query".to_string(),
            format!("https://RELAY.example.com/git/{owner}/demo"),
        ] {
            assert_eq!(
                identity.git_auth(&bad).unwrap_err(),
                IdentityError::NotAllowed,
                "{bad}"
            );
        }
    }

    #[test]
    fn http_auth_is_fresh_each_time() {
        let identity = Identity::ephemeral();
        let a = identity
            .http_auth("https://relay.example.com/query", "POST", Some(b"[]"))
            .unwrap();
        let b = identity
            .http_auth("https://relay.example.com/query", "POST", Some(b"[]"))
            .unwrap();
        assert_ne!(a, b);
        let json = base64::engine::general_purpose::STANDARD
            .decode(a.strip_prefix("Nostr ").unwrap())
            .unwrap();
        let event: Event = serde_json::from_slice(&json).unwrap();
        assert_eq!(event.kind, Kind::HttpAuth);
        let tags: Vec<&str> = event.tags.iter().map(|t| t.kind()).collect();
        assert_eq!(tags, ["u", "method", "payload", "nonce"]);
    }

    #[test]
    fn signs_only_blossom_media_tokens() {
        let identity = Identity::ephemeral();
        let now = Timestamp::now().as_secs();
        let hash = "ab".repeat(32);
        let tag = |name: &str, value: &str| vec![name.to_string(), value.to_string()];
        let upload = [
            tag("x", &hash),
            tag("t", "upload"),
            tag("server", "relay.example.com"),
            tag("expiration", &(now + 60).to_string()),
        ];
        let event = identity
            .sign_media_event(24242, now, &upload, "Upload attachment")
            .unwrap();
        assert!(event.verify().is_ok());
        assert_eq!(event.created_at.as_secs(), now);
        let get = [
            tag("t", "get"),
            tag("server", "127.0.0.1:53000"),
            tag("expiration", &(now + 60).to_string()),
        ];
        assert!(
            identity
                .sign_media_event(24242, now, &get, "Get buzz-media")
                .is_ok()
        );
        let refused = |kind, created_at, tags: &[Vec<String>], content| {
            identity
                .sign_media_event(kind, created_at, tags, content)
                .unwrap_err()
        };
        assert_eq!(
            refused(9, now, &get, "Get buzz-media"),
            IdentityError::NotAllowed
        );
        assert_eq!(
            refused(24242, now, &get, "Upload attachment"),
            IdentityError::NotAllowed
        );
        // A delete, a long-lived token, a stale one, or an extra tag.
        let mut delete = upload.clone();
        delete[1] = tag("t", "delete");
        assert_eq!(
            refused(24242, now, &delete, "Upload attachment"),
            IdentityError::NotAllowed
        );
        let mut long = get.clone();
        long[2] = tag("expiration", &(now + 3600).to_string());
        assert_eq!(
            refused(24242, now, &long, "Get buzz-media"),
            IdentityError::NotAllowed
        );
        assert_eq!(
            refused(24242, now - 3600, &get, "Get buzz-media"),
            IdentityError::NotAllowed
        );
        let mut extra = get.to_vec();
        extra.push(tag("p", &hash));
        assert_eq!(
            refused(24242, now, &extra, "Get buzz-media"),
            IdentityError::NotAllowed
        );
    }
}
