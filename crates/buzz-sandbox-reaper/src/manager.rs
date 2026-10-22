//! The session manager's request admission (`docs/remote-agents.md` §Relay
//! transport): authorize each kind 24210 request, deduplicate it by
//! `request_id`, and run it against the namespace of its verified owner.
//!
//! Admission is synchronous and holds the state lock only briefly; the relay
//! loop publishes the replies it decides and spawns the operations it starts.
//! Nothing here logs: the caller logs an [`Admission`]'s event id, op and
//! code, never content.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use buzz_backend_kubernetes::provider::{self, Target};
use buzz_backend_kubernetes::wire::{ErrorCode, ProviderError, Request, Response};
use buzz_session_control::{open_request, Op, OpenedRequest, WireError};
use nostr::{Event, EventId, Keys, PublicKey, Timestamp};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{is_dns_label, is_owner, CONFIG_VERSION};
use crate::enroll::{self, Enrollment};

/// How long received event ids and finished responses are kept.
pub const REPLAY_WINDOW_SECS: u64 = 600;
/// Received event ids kept at once. Only authorized, fresh events count.
pub const MAX_SEEN: usize = 10_000;
/// Requests tracked at once, running or finished.
pub const MAX_REQUESTS: usize = 1_024;
/// Requests running at once.
pub const MAX_RUNNING: usize = 16;
/// Events waiting for one running request: the first and its retries.
pub const MAX_WAITERS: usize = 8;

// A replay must still be in the cache whenever its `created_at` would pass
// the freshness check.
const _: () = assert!(REPLAY_WINDOW_SECS > 2 * buzz_session_control::MAX_CREATED_AT_SKEW_SECS);
const _: () = assert!(REPLAY_WINDOW_SECS > buzz_session_control::EXPIRATION_SECS);

/// The session manager's configuration (`serve`), parsed from the JSON file
/// mounted from the Terraform-managed ConfigMap `sandbox-manager-config`. It
/// is the only source of the developers the manager serves and of the
/// namespace each one's requests may touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerConfig {
    /// Owner pubkey to its developer.
    developers: HashMap<PublicKey, Developer>,
    /// The bucket every developer's checkpoints go to, under `<id>/`.
    checkpoint_bucket: String,
}

/// One configured developer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Developer {
    /// The Terraform `developers` key.
    pub id: String,
    pub namespace: String,
    /// The secret `enroll` writes the developer's profile to.
    pub profile_secret_arn: String,
    /// The secret's region, from its ARN.
    pub region: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    /// Checked before the schema, in [`ManagerConfig::parse`].
    #[allow(dead_code)]
    version: u32,
    /// Developer ID (the Terraform `developers` key) to its binding.
    developers: BTreeMap<String, DeveloperEntry>,
    /// The checkpoint bucket, shared by every developer under `<id>/`.
    checkpoint_bucket: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeveloperEntry {
    /// The developer's Buzz owner pubkey (64 lowercase hex).
    owner_pubkey: String,
    namespace: String,
    /// Where `enroll` writes the developer's profile.
    profile_secret_arn: String,
}

/// Why the manager configuration was refused. Names fields, never values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagerConfigError {
    #[error("invalid manager config: {0}")]
    Malformed(String),
    #[error("unsupported config version {0} (expected {CONFIG_VERSION})")]
    UnsupportedVersion(u64),
    #[error("developers must list at least one developer")]
    NoDevelopers,
    #[error("checkpoint_bucket is invalid")]
    InvalidCheckpointBucket,
    #[error("developers.{developer}.{field} is invalid")]
    InvalidField {
        developer: String,
        field: &'static str,
    },
    /// The owner selects the namespace, so it must select only one.
    #[error("developers.{developer}.owner_pubkey belongs to another developer too")]
    DuplicateOwner { developer: String },
}

/// An S3 bucket name: 3 to 63 lowercase letters, digits, dots, and hyphens,
/// starting and ending with a letter or digit.
fn is_bucket_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let edge = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    (3..=63).contains(&bytes.len())
        && bytes.iter().all(|b| edge(b) || *b == b'.' || *b == b'-')
        && bytes.first().is_some_and(edge)
        && bytes.last().is_some_and(edge)
}

/// Terraform's developer ID rule: 1-32 of `[a-z0-9-]`, starting and ending
/// with a letter or digit.
fn is_developer_id(id: &str) -> bool {
    id.len() <= 32 && is_dns_label(id)
}

/// `profile.py`'s `secret_region`: the region of a profile secret that
/// belongs to `developer`, `arn:aws:secretsmanager:<region>:<account>:secret:
/// voltage/staging/agents/<developer>/profile-<6 letters or digits>`. Checked
/// at load, so one developer's owner can never reach another's secret.
fn profile_secret_region<'a>(arn: &'a str, developer: &str) -> Option<&'a str> {
    let (region, rest) = arn
        .strip_prefix("arn:aws:secretsmanager:")?
        .split_once(':')?;
    let (account, name) = rest.split_once(":secret:")?;
    let suffix = name
        .strip_prefix("voltage/staging/agents/")?
        .strip_prefix(developer)?
        .strip_prefix("/profile-")?;
    let valid = !region.is_empty()
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && account.len() == 12
        && account.bytes().all(|b| b.is_ascii_digit())
        && suffix.len() == 6
        && suffix.bytes().all(|b| b.is_ascii_alphanumeric());
    valid.then_some(region)
}

impl ManagerConfig {
    /// Parse and validate. The version is checked before the schema, so a
    /// newer file is refused as such.
    pub fn parse(text: &str) -> Result<Self, ManagerConfigError> {
        let malformed = |e: serde_json::Error| ManagerConfigError::Malformed(e.to_string());
        let value: serde_json::Value = serde_json::from_str(text).map_err(malformed)?;
        match value.get("version").and_then(serde_json::Value::as_u64) {
            Some(version) if version == u64::from(CONFIG_VERSION) => {}
            Some(version) => return Err(ManagerConfigError::UnsupportedVersion(version)),
            None => return Err(ManagerConfigError::Malformed("version is required".into())),
        }
        let file: ConfigFile = serde_json::from_value(value).map_err(malformed)?;
        if file.developers.is_empty() {
            return Err(ManagerConfigError::NoDevelopers);
        }
        if !is_bucket_name(&file.checkpoint_bucket) {
            return Err(ManagerConfigError::InvalidCheckpointBucket);
        }
        let mut developers = HashMap::new();
        for (id, developer) in file.developers {
            let invalid = |field| ManagerConfigError::InvalidField {
                developer: id.clone(),
                field,
            };
            if !is_developer_id(&id) {
                return Err(invalid("id"));
            }
            let owner = is_owner(&developer.owner_pubkey)
                .then(|| PublicKey::from_hex(&developer.owner_pubkey).ok())
                .flatten()
                .ok_or_else(|| invalid("owner_pubkey"))?;
            if !is_dns_label(&developer.namespace) {
                return Err(invalid("namespace"));
            }
            let region = profile_secret_region(&developer.profile_secret_arn, &id)
                .ok_or_else(|| invalid("profile_secret_arn"))?
                .to_string();
            let entry = Developer {
                id: id.clone(),
                namespace: developer.namespace,
                profile_secret_arn: developer.profile_secret_arn,
                region,
            };
            if developers.insert(owner, entry).is_some() {
                return Err(ManagerConfigError::DuplicateOwner { developer: id });
            }
        }
        Ok(Self {
            developers,
            checkpoint_bucket: file.checkpoint_bucket,
        })
    }

    /// The developer whose owner key is `owner`.
    pub fn developer(&self, owner: &PublicKey) -> Option<&Developer> {
        self.developers.get(owner)
    }

    /// The launch environment every Sandbox of `developer` gets, whatever the
    /// request sends: where its profile and checkpoints are. The image needs
    /// the profile to start, and a Sandbox deploy is refused without
    /// checkpoint storage.
    pub fn launch_env(&self, developer: &Developer) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("BUZZ_DEVELOPER_ID".to_string(), developer.id.clone()),
            (
                "BUZZ_DEVELOPER_PROFILE_ARN".to_string(),
                developer.profile_secret_arn.clone(),
            ),
            (
                "BUZZ_CHECKPOINT_BUCKET".to_string(),
                self.checkpoint_bucket.clone(),
            ),
            (
                "BUZZ_CHECKPOINT_PREFIX".to_string(),
                format!("{}/", developer.id),
            ),
        ])
    }

    /// How many developers the manager serves.
    pub fn len(&self) -> usize {
        self.developers.len()
    }
}

/// Who a reply goes to: the request's signer, `e`-tagging its event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiter {
    pub requester: PublicKey,
    pub event: EventId,
}

/// One request's idempotency key: its signer and `request_id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestKey {
    signer: PublicKey,
    request_id: String,
}

/// An authorized request ready to run.
pub struct Job {
    pub key: RequestKey,
    pub work: Work,
}

/// What a job runs.
pub enum Work {
    /// A provider protocol request, against the owner's namespace.
    Provider { request: Request, target: Target },
    /// Steps 5 and 6 of enrollment: the GitHub login check and the write.
    Enroll {
        developer: Developer,
        profile: serde_json::Value,
    },
}

impl Work {
    /// Run to the `done` response.
    pub async fn run(self, manager: &Manager) -> serde_json::Value {
        match self {
            Work::Provider { request, target } => {
                response_value(&provider::run(request, &target).await)
            }
            Work::Enroll { developer, profile } => {
                manager.enrollment.run(&developer, profile).await
            }
        }
    }
}

/// What to do with one received event.
pub enum Admission {
    /// No reply is possible or due: the event is not addressed to this
    /// manager, does not verify, or does not decrypt to an envelope.
    Ignore { event: EventId, error: WireError },
    /// Reply `done` with this response and stop.
    Done {
        to: Waiter,
        request_id: String,
        op: Option<Op>,
        /// A refusal carries its `code`.
        response: serde_json::Value,
    },
    /// A duplicate of a running request: reply `accepted`; its `done`
    /// follows when the original finishes.
    Joined {
        to: Waiter,
        request_id: String,
        op: Op,
    },
    /// Reply `accepted`, run the job, then [`Manager::complete`] it.
    Start {
        to: Waiter,
        request_id: String,
        op: Op,
        job: Box<Job>,
    },
}

/// The manager's identity, configuration and request state.
pub struct Manager {
    keys: Keys,
    config: ManagerConfig,
    client: kube::Client,
    /// `provider_config` fields set on every request (`--sandbox-provider-config`).
    defaults: serde_json::Map<String, serde_json::Value>,
    enrollment: Enrollment,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Received event id to when it was received.
    seen: HashMap<EventId, Timestamp>,
    requests: HashMap<RequestKey, Tracked>,
    running: usize,
}

struct Tracked {
    /// SHA-256 of the op and payload, so a reused `request_id` with a
    /// different request is refused instead of answered from another one.
    fingerprint: [u8; 32],
    progress: Progress,
}

enum Progress {
    Running {
        waiters: Vec<Waiter>,
    },
    Done {
        response: serde_json::Value,
        at: Timestamp,
    },
}

pub(crate) fn refusal(code: ErrorCode, message: &str) -> serde_json::Value {
    response_value(&Response::failed(ProviderError::refused(code, message)))
}

/// The response as reply JSON. The response types are plain data, so this
/// cannot fail in practice; a hand-built refusal is still conforming.
pub fn response_value(response: &Response) -> serde_json::Value {
    serde_json::to_value(response).unwrap_or_else(
        |_| serde_json::json!({"ok": false, "error": "could not serialize a response"}),
    )
}

fn op_matches(op: Op, request: &Request) -> bool {
    matches!(
        (op, request),
        (Op::Info, Request::Info) | (Op::Deploy, Request::Deploy(_)) | (Op::Stop, Request::Stop(_))
    )
}

impl Manager {
    pub fn new(
        keys: Keys,
        config: ManagerConfig,
        client: kube::Client,
        enrollment: Enrollment,
    ) -> Self {
        Self {
            keys,
            config,
            client,
            defaults: serde_json::Map::new(),
            enrollment,
            state: Mutex::default(),
        }
    }

    /// Set the `provider_config` fields the manager decides for every
    /// request, replacing the request's own. Checked by
    /// [`provider::check_managed_defaults`] first.
    pub fn with_sandbox_defaults(
        mut self,
        defaults: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        self.defaults = defaults;
        self
    }

    pub fn keys(&self) -> &Keys {
        &self.keys
    }

    /// Authorize `event` and decide its reply (steps in
    /// `docs/remote-agents.md` §Relay transport, in order).
    pub fn admit(&self, event: &Event, now: Timestamp) -> Admission {
        // 1. The signature, addressing, tags and envelope.
        let opened = match open_request(event, &self.keys) {
            Ok(opened) => opened,
            Err(error) => return self.unopened(event, error),
        };
        let op = opened.request.op;
        let to = Waiter {
            requester: opened.signer,
            event: opened.event_id,
        };
        let answer = |response: serde_json::Value| Admission::Done {
            to,
            request_id: opened.request.request_id.clone(),
            op: Some(op),
            response,
        };
        let refused = |code: ErrorCode, message: &str| answer(refusal(code, message));
        // 2 and 7. The owner: attested by the signer, or the signer itself for
        // `enroll`. Either way a configured developer.
        let owner = match self.owner(&opened) {
            Ok(owner) => owner,
            Err((code, message)) => return refused(code, message),
        };
        let Some(developer) = self.config.developer(&owner) else {
            return refused(
                ErrorCode::UnauthorizedOwner,
                "the request's owner is not a developer this manager serves",
            );
        };
        // 3. The payload acts for the signer: its agent key, or for `enroll`
        // a valid profile of the signing owner's developer.
        let work = if op == Op::Enroll {
            match enroll::admit_profile(&opened.request.payload, &owner, developer) {
                Ok(profile) => Work::Enroll {
                    developer: developer.clone(),
                    profile,
                },
                Err((code, message)) => return refused(code, message),
            }
        } else {
            match self.payload(&opened) {
                // 6. The namespace comes from the verified owner, never the request.
                Ok(request) => Work::Provider {
                    request,
                    target: Target::Managed {
                        client: self.client.clone(),
                        namespace: developer.namespace.clone(),
                        agent: opened.signer,
                        owner,
                        defaults: self.defaults.clone(),
                        env: self.config.launch_env(developer),
                    },
                },
                Err(response) => return answer(response),
            }
        };
        // 4. Freshness.
        if opened.check_fresh(now).is_err() {
            return refused(
                ErrorCode::StaleRequest,
                "the request is more than 60 seconds from the manager's clock, or has expired",
            );
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.prune(now);
        // 5. Replay.
        if state.seen.contains_key(&opened.event_id) {
            return refused(
                ErrorCode::ReplayedRequest,
                "this request event was already received; send a new event to retry",
            );
        }
        if state.seen.len() >= MAX_SEEN {
            return refused(ErrorCode::ManagerBusy, "the session manager is busy; retry");
        }
        state.seen.insert(opened.event_id, now);
        // Idempotency by request_id, scoped to the signer.
        let key = RequestKey {
            signer: opened.signer,
            request_id: opened.request.request_id.clone(),
        };
        let fingerprint = fingerprint(&opened);
        if let Some(tracked) = state.requests.get_mut(&key) {
            if tracked.fingerprint != fingerprint {
                return refused(
                    ErrorCode::RequestIdReused,
                    "this request_id was already used for a different request",
                );
            }
            return match &mut tracked.progress {
                Progress::Running { waiters } if waiters.len() >= MAX_WAITERS => {
                    refused(ErrorCode::ManagerBusy, "the session manager is busy; retry")
                }
                Progress::Running { waiters } => {
                    waiters.push(to);
                    Admission::Joined {
                        to,
                        request_id: key.request_id,
                        op,
                    }
                }
                Progress::Done { response, .. } => answer(response.clone()),
            };
        }
        if state.running >= MAX_RUNNING || state.requests.len() >= MAX_REQUESTS {
            return refused(ErrorCode::ManagerBusy, "the session manager is busy; retry");
        }
        state.running += 1;
        state.requests.insert(
            key.clone(),
            Tracked {
                fingerprint,
                progress: Progress::Running { waiters: vec![to] },
            },
        );
        Admission::Start {
            to,
            request_id: key.request_id.clone(),
            op,
            job: Box::new(Job { key, work }),
        }
    }

    /// Record a finished request's response and return everyone waiting for
    /// it. A later duplicate is answered from the record.
    pub fn complete(
        &self,
        key: &RequestKey,
        response: serde_json::Value,
        now: Timestamp,
    ) -> Vec<Waiter> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.running = state.running.saturating_sub(1);
        let Some(tracked) = state.requests.get_mut(key) else {
            return Vec::new();
        };
        let progress =
            std::mem::replace(&mut tracked.progress, Progress::Done { response, at: now });
        match progress {
            Progress::Running { waiters } => waiters,
            Progress::Done { .. } => Vec::new(),
        }
    }

    /// The refusal for an event that verified but could not be fully read,
    /// or [`Admission::Ignore`] when no reply can be addressed.
    fn unopened(&self, event: &Event, error: WireError) -> Admission {
        let (code, request_id) = match &error {
            WireError::UnsupportedVersion { request_id, .. } => {
                (ErrorCode::UnsupportedVersion, request_id.clone())
            }
            WireError::UnsupportedOp { request_id } => {
                (ErrorCode::UnsupportedOp, request_id.clone())
            }
            _ => {
                return Admission::Ignore {
                    event: event.id,
                    error,
                }
            }
        };
        Admission::Done {
            to: Waiter {
                requester: event.pubkey,
                event: event.id,
            },
            request_id,
            op: None,
            response: refusal(code, &error.to_string()),
        }
    }

    /// Steps 2 and 7: the verified owner.
    fn owner(&self, opened: &OpenedRequest) -> Result<PublicKey, (ErrorCode, &'static str)> {
        if opened.request.op == Op::Enroll {
            // Enrollment is owner-level: an agent key's attestation is not enough.
            if opened.auth_tag.is_some() {
                return Err((
                    ErrorCode::SignerMismatch,
                    "enroll must be signed by the owner key itself",
                ));
            }
            return Ok(opened.signer);
        }
        let tag = opened.auth_tag.as_deref().ok_or((
            ErrorCode::UnauthorizedOwner,
            "the request carries no owner attestation",
        ))?;
        buzz_sdk::nip_oa::verify_auth_tag_for_auth_event(
            tag,
            &opened.signer,
            opened.created_at.as_secs(),
        )
        .map_err(|_| {
            (
                ErrorCode::UnauthorizedOwner,
                "the owner attestation does not verify for the signer",
            )
        })
    }

    /// Step 3: parse the provider request and bind its agent key to the
    /// signer. The response is the refusal to send otherwise.
    fn payload(&self, opened: &OpenedRequest) -> Result<Request, serde_json::Value> {
        let request = provider::parse(opened.request.payload.clone())
            .map_err(|response| response_value(&response))?;
        if !op_matches(opened.request.op, &request) {
            return Err(response_value(&Response::error(
                "the request op does not match its payload",
            )));
        }
        let nsec = match &request {
            Request::Deploy(deploy) | Request::Stop(deploy) => &deploy.agent.private_key_nsec,
            Request::Info => return Ok(request),
        };
        let agent = Keys::parse(nsec).map(|keys| keys.public_key());
        if agent.ok() != Some(opened.signer) {
            return Err(refusal(
                ErrorCode::SignerMismatch,
                "the payload's agent key is not the request signer",
            ));
        }
        Ok(request)
    }
}

impl State {
    /// Forget event ids and finished requests older than the replay window.
    fn prune(&mut self, now: Timestamp) {
        let cutoff = now.as_secs().saturating_sub(REPLAY_WINDOW_SECS);
        self.seen.retain(|_, received| received.as_secs() >= cutoff);
        self.requests.retain(|_, tracked| match tracked.progress {
            Progress::Running { .. } => true,
            Progress::Done { at, .. } => at.as_secs() >= cutoff,
        });
    }
}

fn fingerprint(opened: &OpenedRequest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(opened.request.op.as_str());
    hasher.update([0]);
    // A retry carries the same payload, so it serializes identically.
    hasher.update(opened.request.payload.to_string());
    hasher.finalize().into()
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
