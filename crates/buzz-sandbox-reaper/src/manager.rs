//! The session manager's request admission (`docs/remote-agents.md` §Relay
//! transport): authorize each kind 24210 request, deduplicate it by
//! `request_id`, and run it against the namespace of its verified owner.
//!
//! Admission is synchronous and holds the state lock only briefly; the relay
//! loop publishes the replies it decides and spawns the operations it starts.
//! Nothing here logs: the caller logs an [`Admission`]'s event id, op and
//! code, never content.

use crate::codex_login::Change;
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
use crate::mobile_launch::{Registration, Start};

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
    /// Enable only after deploying a matching shared-task agent image.
    shared_tasks: bool,
    /// Provision Paseo daemons at enrollment (`paseo.rs`). Enable after
    /// creating the `buzz-paseo` namespace and Secret Role.
    paseo_daemons: bool,
    /// Where the daemons and the Paseo apps meet, `host:port`, always TLS.
    paseo_relay_endpoint: String,
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
    #[serde(default)]
    shared_tasks: bool,
    #[serde(default)]
    paseo_daemons: bool,
    #[serde(default)]
    paseo_relay_endpoint: Option<String>,
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
    #[error("paseo_relay_endpoint must be host:port, without a scheme or path")]
    InvalidPaseoRelayEndpoint,
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
        let paseo_relay_endpoint = file
            .paseo_relay_endpoint
            .unwrap_or_else(|| crate::paseo::RELAY_ENDPOINT.to_owned());
        if !crate::paseo::is_relay_endpoint(&paseo_relay_endpoint) {
            return Err(ManagerConfigError::InvalidPaseoRelayEndpoint);
        }
        Ok(Self {
            developers,
            checkpoint_bucket: file.checkpoint_bucket,
            shared_tasks: file.shared_tasks,
            paseo_daemons: file.paseo_daemons,
            paseo_relay_endpoint,
        })
    }

    /// Whether enrollment provisions each developer's Paseo daemon.
    pub fn paseo_daemons(&self) -> bool {
        self.paseo_daemons
    }

    /// The Paseo relay endpoint the pairing offers name (`host:port`).
    pub fn paseo_relay_endpoint(&self) -> &str {
        &self.paseo_relay_endpoint
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

    /// Bind the broker to the same configured developers as the relay.
    pub fn developers(&self) -> impl Iterator<Item = Developer> + '_ {
        self.developers.values().cloned()
    }

    /// Each configured developer with the owner key it is bound to.
    pub fn owned_developers(&self) -> impl Iterator<Item = (PublicKey, Developer)> + '_ {
        self.developers
            .iter()
            .map(|(owner, developer)| (*owner, developer.clone()))
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
    /// Reserve one shared task. This operation cannot start or stop a provider.
    Admit {
        owner: PublicKey,
        agent: PublicKey,
        namespace: String,
        request: crate::shared_tasks::Request,
    },
    /// A provider protocol request, against the owner's namespace.
    Provider {
        request: Request,
        target: Target,
        registration: Option<Registration>,
        /// The verified owner's developer, whose Paseo daemon lists the
        /// registered agents.
        developer: Developer,
    },
    /// An owner-authorized start using a registered launch identity.
    Start {
        start: Start,
        target: Target,
        developer: Developer,
    },
    /// Steps 5 and 6 of enrollment: the merge, the GitHub login check and
    /// the write.
    Enroll {
        developer: Developer,
        profile: serde_json::Value,
        login: Option<Change>,
        git: Option<crate::paseo::GitReadToken>,
    },
    /// The `profile` operation: describe the developer's stored profile.
    Profile { developer: Developer },
}

impl Work {
    /// Run to the `done` response.
    pub async fn run(self, manager: &Manager) -> serde_json::Value {
        match self {
            Work::Admit {
                owner,
                agent,
                namespace,
                request,
            } => {
                manager
                    .admit_shared(owner, agent, &namespace, &request)
                    .await
            }
            Work::Provider {
                request,
                target,
                registration,
                developer,
            } => {
                let mut response = response_value(&provider::run(request, &target).await);
                if response["ok"] == true {
                    if let (
                        Some(registration),
                        Target::Managed {
                            client, namespace, ..
                        },
                    ) = (registration, &target)
                    {
                        if let Err(error) = registration.save(client.clone(), namespace).await {
                            return error;
                        }
                        if let Err(error) = manager.enrollment.agents_changed(&developer).await {
                            // Deployment and its registration already succeeded. Keep
                            // that result authoritative; only the derived daemon list
                            // needs recovery from the durable registration on enroll.
                            response["paseo_sync"] = error;
                        }
                    }
                }
                response
            }
            Work::Start {
                start,
                target,
                developer,
            } => {
                let Target::Managed {
                    client, namespace, ..
                } = &target
                else {
                    return refusal(
                        ErrorCode::InvalidStart,
                        "mobile starts require a managed target",
                    );
                };
                let request = match start.request(client.clone(), namespace).await {
                    Ok(request) => request,
                    Err(error) => return error,
                };
                let response = response_value(&provider::run(request, &target).await);
                manager.finish_start(&start, &developer, response).await
            }
            Work::Enroll {
                developer,
                profile,
                login,
                git,
            } => match (login, git) {
                (login, Some(git)) => {
                    manager
                        .enrollment
                        .run_with_daemon(&developer, profile, login, Some(git))
                        .await
                }
                (Some(login), None) => {
                    manager
                        .enrollment
                        .run_with_login(&developer, profile, Some(login))
                        .await
                }
                (None, None) => manager.enrollment.run(&developer, profile).await,
            },
            Work::Profile { developer } => manager.enrollment.describe(&developer).await,
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
    /// Registry discovery uses only the manager's configured namespaces.
    pub(crate) fn mention_bindings(&self) -> impl Iterator<Item = (PublicKey, &Developer)> {
        self.config
            .developers
            .iter()
            .map(|(owner, developer)| (*owner, developer))
    }

    /// Share the confined manager's existing Kubernetes client with its inbox.
    pub(crate) fn cluster(&self) -> kube::Client {
        self.client.clone()
    }

    /// Sharing expands task invocation only when the matching harness rollout
    /// is enabled. Owner lifecycle operations retain their separate checks.
    pub(crate) fn sharing_enabled(&self) -> bool {
        self.config.shared_tasks
    }

    /// Start the agent for one mention. The trigger's signer is the owner, a
    /// live owner-delegated gateway, a relay-proven gateway workflow owner
    /// (`delegation`, freshly verified for this trigger), or a shared requester.
    ///
    /// When the thread's session has ended, a mention signed by the owner or
    /// a live delegated gateway is the explicit recovery request a desktop
    /// recovery action would send: the provider restores the tombstone's
    /// checkpoint if the session ended before the mention, and the recovered
    /// session replays it. A relay-signed scheduled workflow prompt or a shared
    /// requester's mention never recovers a session; it gets the ordinary
    /// refusal.
    pub(crate) async fn start_mention(
        &self,
        owner: PublicKey,
        agent: PublicKey,
        relay: &str,
        event: &Event,
        delegation: Option<&crate::mobile_launch::GatewayRequest>,
    ) -> serde_json::Value {
        let Some(developer) = self.config.developer(&owner) else {
            return refusal(ErrorCode::UnauthorizedOwner, "owner is not configured");
        };
        let payload =
            serde_json::json!({"agent_pubkey": agent, "relay_url": relay, "trigger": event});
        let delegation = delegation.filter(|request| request.delegation.owner() == owner);
        let (admitted, recovers) = if event.pubkey == owner {
            (
                Start::admit(payload, owner, Timestamp::now().as_secs()),
                true,
            )
        } else if let Some(delegation) = delegation {
            // A scheduled workflow prompt is signed by the relay, not the
            // gateway. It is not a person asking to restore the thread.
            (
                Start::gateway(payload, delegation, Timestamp::now().as_secs()),
                delegation.workflow_relay.is_none(),
            )
        } else {
            let request = crate::shared_tasks::Request {
                relay_url: relay.into(),
                trigger: event.clone(),
            };
            let admitted = match self
                .authorize_shared(owner, agent, &developer.namespace, &request)
                .await
            {
                Ok(task) => Start::shared(task, relay, event),
                Err(error) => Err(error),
            };
            (admitted, false)
        };
        let start = match admitted {
            Ok(start) => start,
            Err(error) => return error,
        };
        let target = Target::Managed {
            client: self.client.clone(),
            namespace: developer.namespace.clone(),
            agent,
            owner,
            defaults: self.defaults.clone(),
            env: self.shared_env(developer),
        };
        let launch = |request: Result<Request, serde_json::Value>| async {
            match request {
                Ok(request) => {
                    let response = response_value(&provider::run(request, &target).await);
                    self.finish_start(&start, developer, response).await
                }
                Err(error) => error,
            }
        };
        let response = launch(
            start
                .request(self.client.clone(), &developer.namespace)
                .await,
        )
        .await;
        // An ordinary deploy never resumes an ended session. `recovery_interrupted`
        // means a recovery deleted the tombstone and stopped; recovering again
        // finishes it from the provider's recovery journal.
        let ended = matches!(
            response["code"].as_str(),
            Some("session_ended" | "recovery_interrupted")
        );
        if !(recovers && ended) {
            return response;
        }
        launch(
            start
                .recovery(self.client.clone(), &developer.namespace)
                .await,
        )
        .await
    }

    /// Preserve a successful start and refresh its derived Paseo provider list.
    async fn finish_start(
        &self,
        start: &Start,
        developer: &Developer,
        mut response: serde_json::Value,
    ) -> serde_json::Value {
        if response["ok"] == true {
            // The durable registration permits a later start/enroll to retry
            // this refresh. Never report a committed deploy as failed.
            let synced = match start
                .remember_channel(self.client.clone(), &developer.namespace)
                .await
            {
                Ok(_) => self.enrollment.agents_changed(developer).await,
                Err(error) => Err(error),
            };
            if let Err(error) = synced {
                response["paseo_sync"] = error;
            }
        }
        response
    }

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
            let login = match Change::parse(opened.request.payload.get("codex_login")) {
                Ok(login) => login,
                Err(error) => return refused(ErrorCode::InvalidProfile, &error.to_string()),
            };
            let git = match crate::paseo::GitReadToken::parse(opened.request.payload.get("paseo")) {
                Ok(git) => git,
                Err(_) => return refused(ErrorCode::InvalidProfile, "invalid daemon Git token"),
            };
            match enroll::admit_profile(&opened.request.payload, &owner, developer) {
                Ok(profile) => Work::Enroll {
                    developer: developer.clone(),
                    profile,
                    login,
                    git,
                },
                Err((code, message)) => return refused(code, message),
            }
        } else if op == Op::Profile {
            if opened.request.payload != serde_json::json!({}) {
                return refused(ErrorCode::InvalidProfile, "the profile payload must be {}");
            }
            Work::Profile {
                developer: developer.clone(),
            }
        } else if op == Op::Admit {
            if !self.config.shared_tasks {
                return answer(
                    serde_json::json!({"ok":false,"code":"shared_task_denied","error":"shared tasks are not enabled on this manager"}),
                );
            }
            let request = match serde_json::from_value(opened.request.payload.clone()) {
                Ok(request) => request,
                Err(_) => {
                    return answer(
                        serde_json::json!({"ok":false,"code":"shared_task_denied","error":"admit requires relay_url and the original signed trigger"}),
                    )
                }
            };
            Work::Admit {
                owner,
                agent: opened.signer,
                namespace: developer.namespace.clone(),
                request,
            }
        } else if op == Op::Start {
            let start = match Start::admit(
                opened.request.payload.clone(),
                owner,
                opened.created_at.as_secs(),
            ) {
                Ok(start) => start,
                Err(response) => return answer(response),
            };
            let target = Target::Managed {
                client: self.client.clone(),
                namespace: developer.namespace.clone(),
                agent: start.agent(),
                owner,
                defaults: self.defaults.clone(),
                env: self.shared_env(developer),
            };
            Work::Start {
                start,
                target,
                developer: developer.clone(),
            }
        } else {
            let registration = if op == Op::Deploy {
                match Registration::new(
                    owner,
                    opened.signer,
                    opened.request.payload["agent"].clone(),
                ) {
                    Ok(registration) => {
                        Some(registration.in_channel(&opened.request.payload["provider_config"]))
                    }
                    Err(response) => return answer(response),
                }
            } else {
                None
            };
            match self.payload(&opened) {
                // 6. The namespace comes from the verified owner, never the request.
                Ok(request) => {
                    let thread = match &request {
                        Request::Deploy(d) | Request::Stop(d) => {
                            d.provider_config["sandbox"]["thread_root"].is_string()
                        }
                        Request::Info => false,
                    };
                    let env = if thread {
                        self.shared_env(developer)
                    } else {
                        self.config.launch_env(developer)
                    };
                    Work::Provider {
                        request,
                        registration,
                        developer: developer.clone(),
                        target: Target::Managed {
                            client: self.client.clone(),
                            namespace: developer.namespace.clone(),
                            agent: opened.signer,
                            owner,
                            defaults: self.defaults.clone(),
                            env,
                        },
                    }
                }
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
                };
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
        if matches!(opened.request.op, Op::Enroll | Op::Profile | Op::Start) {
            // The profile is owner-level: an agent key's attestation is not enough.
            if opened.auth_tag.is_some() {
                return Err((
                    ErrorCode::SignerMismatch,
                    "enroll, profile and start must be signed by the owner key itself",
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

    fn shared_env(&self, developer: &Developer) -> BTreeMap<String, String> {
        let mut env = self.config.launch_env(developer);
        if self.config.shared_tasks {
            env.insert(
                "BUZZ_ACP_SHARING_MANAGER".into(),
                self.keys.public_key().to_hex(),
            );
        }
        env
    }

    pub(crate) async fn admit_shared(
        &self,
        owner: PublicKey,
        agent: PublicKey,
        namespace: &str,
        request: &crate::shared_tasks::Request,
    ) -> serde_json::Value {
        match self
            .authorize_shared(owner, agent, namespace, request)
            .await
        {
            Ok(task) => {
                serde_json::json!({"ok":true,"request":task.origin.request,"origin_agent":task.origin.agent,
                "human":task.human,"channel":task.scope.channel,"thread_root":task.scope.root})
            }
            Err(error) => error,
        }
    }

    async fn authorize_shared(
        &self,
        owner: PublicKey,
        agent: PublicKey,
        namespace: &str,
        request: &crate::shared_tasks::Request,
    ) -> Result<buzz_sdk::sharing::policy::Task, serde_json::Value> {
        use buzz_sdk::sharing::policy::Error as PolicyError;
        if !self.config.shared_tasks {
            return Err(
                serde_json::json!({"ok":false,"code":"shared_task_denied","error":"shared tasks are not enabled on this manager"}),
            );
        }
        let result=tokio::time::timeout(std::time::Duration::from_secs(40),async {
            let developers = self.config.owned_developers().take(crate::paseo::MAX_DAEMON_IDENTITIES + 1).collect::<Vec<_>>();
            let daemon_keys = self.enrollment.shared_daemon_keys(&developers).await.map_err(|error| ("identity_snapshot", error))?;
            let task=match crate::shared_tasks::verify(self.client.clone(),namespace,owner,agent,request,&daemon_keys).await {
                Ok(task)=>task,
                Err(crate::shared_tasks::Failure{stage,error:PolicyError::Denied(reason)})=>return Err((stage,serde_json::json!({"ok":false,"code":"shared_task_denied","error":reason}))),
                Err(crate::shared_tasks::Failure{stage,error:PolicyError::Unavailable})=>return Err((stage,serde_json::json!({"ok":false,"code":"shared_task_retry","error":"authorization lookup failed temporarily; permission has not been determined; retry this task"}))),
            };
            Enrollment::check_shared_human(&daemon_keys, &task).map_err(|error| ("managed_gateway", error))?;
            let Some(origin)=self.config.developer(&task.origin_owner) else {
                return Err(("origin_owner",serde_json::json!({"ok":false,"code":"shared_task_denied","error":"the initial agent must use this session manager"})));
            };
            match crate::shared_budget::admit(self.client.clone(),&origin.namespace,&request.relay_url,&task,Timestamp::now().as_secs()).await {
                Ok(())=>{
                    if let Some(gateway)=task.gateway {
                        tracing::info!(%gateway, human = %task.human, %agent, trigger = %request.trigger.id,
                            "shared task admitted on behalf of a delegated gateway's owner");
                    }
                    Ok(task)
                }
                Err(error)=>{
                    let code=if matches!(error,crate::shared_budget::Error::Storage) {"shared_task_retry"} else {"shared_task_denied"};
                    Err(("budget",serde_json::json!({"ok":false,"code":code,"error":error.to_string()})))
                }
            }
        }).await;
        let result = result.unwrap_or_else(|_| Err(("timeout",serde_json::json!({"ok":false,"code":"shared_task_retry","error":"task authorization timed out; retry this task"}))));
        result.map_err(|(stage, mut response)| {
            let retryable = response["code"] == "shared_task_retry";
            response["retryable"] = serde_json::json!(retryable);
            // Fixed refusal strings and public identifiers only: never task text.
            tracing::warn!(stage, code = %response["code"], reason = %response["error"],
                %owner, %agent, trigger = %request.trigger.id, "shared task admission refused");
            response
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
