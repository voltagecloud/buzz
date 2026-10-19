//! Agent Sandbox integration. The upstream controller owns the Pod; this
//! provider owns its launch contract and binds credentials to the first Pod UID.
//!
//! Lifecycle (`docs/remote-agents.md` §Sandbox lifecycle): a session that ended
//! — stopped, completed, replaced, lost, or abandoned while binding — is
//! tombstoned by `buzz-sandbox-reaper` and is never redeployed by an ordinary
//! deploy.

use std::collections::BTreeMap;
use std::time::Duration;

use buzz_backend_kubernetes::lifecycle::{
    annotation, owned_pod, sandbox_resource, EndedReason, Lifecycle, OperatingMode, CHANNEL,
    CHECKPOINTING, CHECKPOINTING_DISABLED, CHECKPOINTING_ENABLED, ENDED_REASON, GENERATION,
    INITIAL_POD, OWNER, SANDBOX_BINDING_VERSION, SESSION, THREAD,
};
use k8s_openapi::api::core::v1::{EnvVar, EnvVarSource, ObjectFieldSelector, Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, ListParams, PostParams};
use kube::core::DynamicObject;
use kube::{Client, ResourceExt};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::wire::{ErrorCode, ProviderError};
use crate::{config::ProviderConfig, intent::Fingerprint, naming, pod};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    pub channel_id: String,
    pub thread_root: String,
}

impl Options {
    fn name(&self, identity: &naming::AgentIdentity) -> String {
        let scope = Sha256::digest(format!("{}:{}", self.channel_id, self.thread_root).as_bytes());
        format!("{}-{}", identity.pod_name(), &hex::encode(scope)[..12])
    }

    pub fn parse(value: &serde_json::Value) -> Result<Option<Self>, String> {
        let Some(value) = value.get("sandbox") else {
            return Ok(None);
        };
        if value == &serde_json::Value::Bool(false) {
            return Ok(None);
        }
        let options: Self = serde_json::from_value(value.clone())
            .map_err(|_| "sandbox requires channel_id and canonical thread_root".to_string())?;
        if uuid::Uuid::parse_str(&options.channel_id)
            .map(|id| id.to_string())
            .ok()
            .as_deref()
            != Some(&options.channel_id)
        {
            return Err("sandbox channel_id must be a canonical UUID".into());
        }
        if options.thread_root.len() != 64
            || !options
                .thread_root
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err("sandbox thread_root must be a lowercase 64-character event ID".into());
        }
        Ok(Some(options))
    }
}

fn api(client: Client, namespace: &str) -> Api<DynamicObject> {
    Api::namespaced_with(client, namespace, &sandbox_resource())
}

fn reason_is(error: &kube::Error, reason: &str) -> bool {
    matches!(error, kube::Error::Api(e) if e.reason == reason)
}

/// Session-specific inputs to [`build`] beyond the launch contract.
struct NewSession<'a> {
    generation: &'a str,
    checkpointing: bool,
}

fn build(
    identity: &naming::AgentIdentity,
    cfg: &ProviderConfig,
    scope: &Options,
    owner: &str,
    session: &NewSession<'_>,
    fingerprint: &Fingerprint,
) -> Result<DynamicObject, String> {
    let generation = session.generation;
    let name = scope.name(identity);
    let mut child = pod::build_pod(identity, cfg, generation, fingerprint);
    child.metadata.name = Some(name.clone());
    // Older bare-Pod providers must refuse these controller-owned resources.
    child.labels_mut().insert(
        naming::LABEL_BINDING_VERSION.into(),
        SANDBOX_BINDING_VERSION.into(),
    );
    let annotations = child.metadata.annotations.get_or_insert_with(BTreeMap::new);
    annotations.extend([
        (OWNER.into(), owner.into()),
        (CHANNEL.into(), scope.channel_id.clone()),
        (THREAD.into(), scope.thread_root.clone()),
        (SESSION.into(), generation.into()),
        (GENERATION.into(), generation.into()),
        (
            CHECKPOINTING.into(),
            if session.checkpointing {
                CHECKPOINTING_ENABLED
            } else {
                CHECKPOINTING_DISABLED
            }
            .into(),
        ),
    ]);
    // The bare-Pod grace (`TERMINATION_GRACE_SECONDS`) already covers the
    // drain and the harness's 120s checkpoint; keep it so the template
    // matches the fingerprinted intent.
    let spec = child.spec.as_mut().ok_or("agent Pod has no spec")?;
    // The harness writes its checkpoint receipt here; the reaper reads it
    // back from the container status (and cross-checks it against S3).
    spec.containers[0].termination_message_policy = Some("File".into());
    spec.containers[0]
        .env
        .get_or_insert_with(Vec::new)
        .push(EnvVar {
            name: "BUZZ_SANDBOX_POD_UID".into(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    api_version: Some("v1".into()),
                    field_path: "metadata.uid".into(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
    let mut sandbox = DynamicObject::new(&name, &sandbox_resource());
    sandbox.metadata = child.metadata.clone();
    sandbox.data = json!({"spec": {
        "operatingMode": "Running",
        "service": false,
        "podTemplate": {
            "metadata": {"labels": child.metadata.labels, "annotations": child.metadata.annotations},
            "spec": child.spec,
        },
    }});
    Ok(sandbox)
}

/// The Sandbox belongs to this agent, owner, and thread. Recovery cannot fix a
/// mismatch here, so the error does not offer it.
fn verify_identity(
    sandbox: &DynamicObject,
    identity: &naming::AgentIdentity,
    scope: &Options,
    owner: &str,
) -> Result<(), ProviderError> {
    let labels = sandbox.labels();
    let marked = labels.get(naming::LABEL_MANAGED_BY).map(String::as_str)
        == Some(naming::MANAGED_BY)
        && labels
            .get(naming::LABEL_BINDING_VERSION)
            .map(String::as_str)
            == Some(SANDBOX_BINDING_VERSION);
    let matches = [
        (naming::ANNOTATION_PUBKEY_FULL, identity.pubkey_hex()),
        (OWNER, owner),
        (CHANNEL, scope.channel_id.as_str()),
        (THREAD, scope.thread_root.as_str()),
    ]
    .into_iter()
    .all(|(key, expected)| annotation(sandbox, key) == Some(expected));
    if !marked || !matches {
        return Err(ProviderError::refused(
            ErrorCode::IdentityMismatch,
            "Sandbox belongs to a different agent, owner, or thread",
        ));
    }
    Ok(())
}

/// The Sandbox is a live session this deploy may reuse. Every refusal names the
/// lifecycle state so the desktop can offer the right explicit action.
fn verify_live(sandbox: &DynamicObject, fingerprint: &Fingerprint) -> Result<(), ProviderError> {
    let refused = |code, message: &str| Err(ProviderError::refused(code, message));
    if sandbox.metadata.deletion_timestamp.is_some() {
        return refused(
            ErrorCode::SessionDeleting,
            "Sandbox is being deleted; retry once it is gone",
        );
    }
    match Lifecycle::of(sandbox) {
        Lifecycle::Ended => {
            // The reason comes from the cluster; only a known spelling is
            // repeated to the user.
            let message = match annotation(sandbox, ENDED_REASON).and_then(EndedReason::parse) {
                Some(reason) => format!(
                    "Sandbox session ended ({}); explicit recovery is required",
                    reason.as_str()
                ),
                None => "Sandbox session ended; explicit recovery is required".into(),
            };
            return refused(ErrorCode::SessionEnded, &message);
        }
        Lifecycle::Ending => {
            return refused(
                ErrorCode::SessionEnding,
                "Sandbox session is ending; recover it explicitly once it has ended",
            )
        }
        Lifecycle::Unrecognized => {
            return refused(
                ErrorCode::LifecycleUnknown,
                "Sandbox has a lifecycle this provider does not know; update Buzz",
            )
        }
        Lifecycle::Live => {}
    }
    if OperatingMode::of(sandbox) != OperatingMode::Running {
        return refused(
            ErrorCode::SessionNotRunning,
            "Sandbox is not an active managed session; explicit recovery is required",
        );
    }
    if annotation(sandbox, naming::ANNOTATION_CREATE_INTENT) != Some(fingerprint.as_str()) {
        return refused(
            ErrorCode::ConfigDiffers,
            "Sandbox launch configuration differs; end the session, then explicit recovery is required",
        );
    }
    Ok(())
}

fn configure_setup(
    cfg: &mut ProviderConfig,
    identity: &naming::AgentIdentity,
    env: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let Some(mode) = env.get("BUZZ_SETUP_MODE") else {
        return Ok(());
    };
    if mode != "codex-device-v1"
        || env.get("BUZZ_ACP_AGENT_COMMAND").map(String::as_str) != Some("codex-acp")
        || env.get("BUZZ_ACP_PERMISSION_MODE").map(String::as_str) != Some("agent-full-access")
    {
        return Err(
            "Sandbox account setup requires codex-device-v1, codex-acp, and agent-full-access"
                .into(),
        );
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "could not establish account setup time")?
        .as_secs();
    let (_, digest) = cfg
        .image
        .as_str()
        .rsplit_once('@')
        .ok_or("image digest is missing")?;
    // The image acknowledges this exact Pod/agent/image generation. The Pod UID
    // comes from the downward API, overriding any caller-supplied environment.
    cfg.pod_options.setup_pending = true;
    env.remove("BUZZ_SETUP_POD_UID");
    env.extend([
        (
            "BUZZ_SETUP_AGENT_PUBKEY".into(),
            identity.pubkey_hex().into(),
        ),
        ("BUZZ_SETUP_IMAGE_DIGEST".into(), digest.into()),
        (
            "BUZZ_SETUP_NONCE".into(),
            uuid::Uuid::new_v4().simple().to_string(),
        ),
        ("BUZZ_SETUP_DEADLINE".into(), (now + 900).to_string()),
        ("CODEX_HOME".into(), "/home/agent/.codex".into()),
        (
            "CODEX_CONFIG".into(),
            r#"{"cli_auth_credentials_store":"file"}"#.into(),
        ),
        ("NO_BROWSER".into(), "1".into()),
    ]);
    Ok(())
}

/// Per-deploy handles and invariants.
struct Session<'a> {
    sandboxes: Api<DynamicObject>,
    pods: Api<Pod>,
    secrets: Api<Secret>,
    name: String,
    identity: &'a naming::AgentIdentity,
    scope: &'a Options,
    owner: &'a str,
    fingerprint: Fingerprint,
}

impl<'a> Session<'a> {
    fn new(
        client: Client,
        namespace: &str,
        identity: &'a naming::AgentIdentity,
        scope: &'a Options,
        owner: &'a str,
        fingerprint: Fingerprint,
    ) -> Self {
        Self {
            sandboxes: api(client.clone(), namespace),
            pods: Api::namespaced(client.clone(), namespace),
            secrets: Api::namespaced(client, namespace),
            name: scope.name(identity),
            identity,
            scope,
            owner,
            fingerprint,
        }
    }
}

/// Reuses a running, identically scoped Sandbox. Ended, replaced, or lost
/// sessions are never redeployed implicitly. Namespace creation remains
/// Terraform's concern.
pub async fn deploy(
    client: Client,
    identity: &naming::AgentIdentity,
    cfg: &ProviderConfig,
    scope: &Options,
    owner: &str,
    mut env: BTreeMap<String, String>,
) -> Result<String, ProviderError> {
    let mut effective_cfg = cfg.clone();
    configure_setup(&mut effective_cfg, identity, &mut env)?;
    let cfg = &effective_cfg;
    if cfg.pod_options.active_deadline_seconds.is_none() {
        return Err("Sandbox requires pod_options.active_deadline_seconds".into());
    }
    let checkpointing = apply_session_env(scope, &mut env)?;
    let fingerprint = pod::intent_template(cfg, env.keys().cloned()).fingerprint();
    let session = Session::new(client, &cfg.namespace, identity, scope, owner, fingerprint);
    let existing = session
        .sandboxes
        .get_opt(&session.name)
        .await
        .map_err(|e| format!("read Sandbox: {e}"))?;
    let sandbox = match existing {
        Some(existing) => existing,
        None => create_new(&session, cfg, checkpointing).await?,
    };
    verify_identity(&sandbox, identity, scope, owner)?;
    verify_live(&sandbox, &session.fingerprint)?;
    bind(&session, cfg, sandbox, env).await
}

/// Opt-out for launches that knowingly run without a stop checkpoint.
const CHECKPOINT_OPT_OUT: &str = "BUZZ_SANDBOX_CHECKPOINT";

/// Status reason and code only: an apiserver message can echo object data,
/// and these calls handle the launch Secret.
fn api_failure(error: &kube::Error) -> String {
    match error {
        kube::Error::Api(status) => format!("{} ({})", status.reason, status.code),
        other => other.to_string(),
    }
}

/// Thread-session env shared by every deploy. Returns whether
/// checkpoint-on-stop is configured. The workspace is an `emptyDir` on a
/// `restartPolicy: Never` Pod, so the stop checkpoint is the only copy that
/// outlives the process; refuse to launch without its storage unless the
/// caller opts out explicitly.
fn apply_session_env(scope: &Options, env: &mut BTreeMap<String, String>) -> Result<bool, String> {
    let opted_out = env.get(CHECKPOINT_OPT_OUT).map(String::as_str) == Some("disabled");
    // A blank value is as unusable as a missing one: the save would fail at stop.
    let configured = |key: &str| env.get(key).is_some_and(|value| !value.trim().is_empty());
    let checkpointing =
        !opted_out && configured("BUZZ_CHECKPOINT_BUCKET") && configured("BUZZ_CHECKPOINT_PREFIX");
    if !opted_out && !checkpointing {
        return Err(format!(
            "Sandbox requires BUZZ_CHECKPOINT_BUCKET and BUZZ_CHECKPOINT_PREFIX in the agent \
             environment, or {CHECKPOINT_OPT_OUT}=disabled to launch without a stop checkpoint"
        ));
    }
    env.insert("BUZZ_ACP_ACTIVITY_LOG".into(), "true".into());
    env.insert("BUZZ_ACP_RESPOND_TO".into(), "owner".into());
    env.insert("BUZZ_ACP_ALLOWED_RESPOND_TO".into(), "owner".into());
    env.insert(
        "BUZZ_ACP_BOUND_SESSION".into(),
        json!({"channel_id":scope.channel_id,"thread_root":scope.thread_root}).to_string(),
    );
    env.insert("BUZZ_ACP_SESSION_POLICY".into(), "thread".into());
    env.insert("BUZZ_ACP_SUBSCRIBE".into(), "mentions".into());
    env.insert("BUZZ_ACP_CHANNELS".into(), scope.channel_id.clone());
    env.insert("BUZZ_ACP_HEARTBEAT_INTERVAL".into(), "0".into());
    if checkpointing {
        env.insert("BUZZ_ACP_CHECKPOINT_ON_STOP".into(), "true".into());
    }
    // Include the UID binding key in the fingerprint without hashing its value.
    env.insert("BUZZ_SANDBOX_INITIAL_POD_UID".into(), String::new());
    Ok(checkpointing)
}

async fn create_new(
    session: &Session<'_>,
    cfg: &ProviderConfig,
    checkpointing: bool,
) -> Result<DynamicObject, ProviderError> {
    let identity = session.identity;
    if session
        .pods
        .get_opt(&session.name)
        .await
        .map_err(|e| format!("check Pod collision: {e}"))?
        .is_some()
        || !session
            .pods
            .list(&ListParams::default().labels(&identity.selector()))
            .await
            .map_err(|e| format!("check existing agent consumers: {e}"))?
            .items
            .iter()
            .all(|pod| {
                pod.labels()
                    .get(naming::LABEL_BINDING_VERSION)
                    .map(String::as_str)
                    == Some(SANDBOX_BINDING_VERSION)
                    && pod.annotations().get(OWNER).map(String::as_str) == Some(session.owner)
                    && pod.owner_references().iter().any(|r| {
                        r.api_version == "agents.x-k8s.io/v1beta1"
                            && r.kind == "Sandbox"
                            && r.controller == Some(true)
                    })
            })
    {
        return Err(ProviderError::refused(
            ErrorCode::AgentPodExists,
            "agent already has a Pod; stop and recover it explicitly before using Sandbox",
        ));
    }
    let generation = uuid::Uuid::new_v4().simple().to_string();
    let new = NewSession {
        generation: &generation,
        checkpointing,
    };
    let desired = build(
        identity,
        cfg,
        session.scope,
        session.owner,
        &new,
        &session.fingerprint,
    )?;
    match session
        .sandboxes
        .create(&PostParams::default(), &desired)
        .await
    {
        Ok(created) => Ok(created),
        Err(e) if reason_is(&e, "AlreadyExists") => session
            .sandboxes
            .get(&session.name)
            .await
            .map_err(|e| format!("read concurrent Sandbox: {e}").into()),
        Err(e) => Err(format!(
            "create Sandbox (install the Agent Sandbox v1beta1 CRD/controller first): {e}"
        )
        .into()),
    }
}

/// Bind credentials to the Sandbox's first Pod and wait for it to start.
async fn bind(
    session: &Session<'_>,
    cfg: &ProviderConfig,
    mut sandbox: DynamicObject,
    env: BTreeMap<String, String>,
) -> Result<String, ProviderError> {
    let identity = session.identity;
    let name = &session.name;
    let generation = annotation(&sandbox, GENERATION)
        .ok_or("Sandbox generation is missing")?
        .to_string();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let Some(child) = session
            .pods
            .get_opt(name)
            .await
            .map_err(|e| format!("read Sandbox Pod: {e}"))?
        else {
            if annotation(&sandbox, INITIAL_POD).is_some() {
                return Err(ProviderError::refused(
                    ErrorCode::PodLost,
                    "Sandbox lost its original Pod; explicit recovery is required",
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(
                    "Sandbox controller has not created its Pod; inspect the Sandbox and retry"
                        .into(),
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        if !owned_pod(&child, &sandbox) {
            return Err(ProviderError::refused(
                ErrorCode::ForeignPod,
                "Pod is not owned by this Sandbox",
            ));
        }
        let uid = child.metadata.uid.as_deref().ok_or("Pod UID is missing")?;
        if child.metadata.deletion_timestamp.is_some()
            || child
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .is_some_and(|p| matches!(p, "Failed" | "Succeeded"))
        {
            return Err(ProviderError::refused(
                ErrorCode::PodEnded,
                "Sandbox session ended; explicit recovery is required",
            ));
        }
        let Some(initial_uid) = annotation(&sandbox, INITIAL_POD) else {
            // resourceVersion makes this an atomic first-Pod binding. A
            // concurrent deploy must retry and observe the winner's binding.
            sandbox
                .annotations_mut()
                .insert(INITIAL_POD.into(), uid.into());
            sandbox = match session
                .sandboxes
                .replace(name, &PostParams::default(), &sandbox)
                .await
            {
                Ok(bound) => bound,
                Err(e) if reason_is(&e, "Conflict") => {
                    sandbox = refresh(session, &generation).await?;
                    if tokio::time::Instant::now() >= deadline {
                        return Err(
                            "Sandbox kept changing during credential binding; retry deploy".into(),
                        );
                    }
                    continue;
                }
                Err(e) => return Err(format!("bind Sandbox to its first Pod: {e}").into()),
            };
            let secret = bound_secret(identity, cfg, &generation, name, uid, env);
            session
                .secrets
                .create(&PostParams::default(), &secret)
                .await
                .map_err(|e| {
                    ProviderError::refused(
                        ErrorCode::BindingIncomplete,
                        format!(
                            "could not create bound Sandbox credentials ({}); explicit recovery is required",
                            api_failure(&e)
                        ),
                    )
                })?;
            return wait_for_start(&session.pods, &sandbox, name, uid).await;
        };
        if uid != initial_uid {
            return Err(ProviderError::refused(
                ErrorCode::PodReplaced,
                "Sandbox Pod was replaced; credentials remain bound to the original session, explicit recovery is required",
            ));
        }
        let secret = session
            .secrets
            .get_opt(&identity.secret_name(&generation))
            .await
            .map_err(|e| format!("could not check Sandbox launch Secret: {}", api_failure(&e)))?;
        let Some(secret) = secret else {
            if tokio::time::Instant::now() >= deadline {
                return Err(ProviderError::refused(
                    ErrorCode::BindingIncomplete,
                    "Sandbox credential binding is incomplete; explicit recovery is required",
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        };
        check_secret(&secret, identity, uid, &env)?;
        return wait_for_start(&session.pods, &sandbox, name, uid).await;
    }
}

/// Re-read a Sandbox after a lost CAS and re-check what the binding relies on.
async fn refresh(session: &Session<'_>, generation: &str) -> Result<DynamicObject, ProviderError> {
    let sandbox = session
        .sandboxes
        .get(&session.name)
        .await
        .map_err(|e| format!("refresh Sandbox binding: {e}"))?;
    verify_identity(&sandbox, session.identity, session.scope, session.owner)?;
    verify_live(&sandbox, &session.fingerprint)?;
    if annotation(&sandbox, GENERATION) != Some(generation) {
        return Err(ProviderError::refused(
            ErrorCode::SessionReplaced,
            "Sandbox generation changed; explicit recovery is required",
        ));
    }
    Ok(sandbox)
}

fn bound_secret(
    identity: &naming::AgentIdentity,
    cfg: &ProviderConfig,
    generation: &str,
    name: &str,
    uid: &str,
    mut env: BTreeMap<String, String>,
) -> Secret {
    env.insert("BUZZ_MANAGED_AGENT_START_NONCE".into(), generation.into());
    env.insert("BUZZ_SANDBOX_INITIAL_POD_UID".into(), uid.into());
    let mut secret = pod::build_secret(identity, &cfg.namespace, generation, env);
    secret.labels_mut().insert(
        naming::LABEL_BINDING_VERSION.into(),
        SANDBOX_BINDING_VERSION.into(),
    );
    secret.metadata.owner_references = Some(vec![OwnerReference {
        api_version: "v1".into(),
        kind: "Pod".into(),
        name: name.into(),
        uid: uid.into(),
        controller: Some(true),
        block_owner_deletion: Some(false),
    }]);
    secret
}

fn check_secret(
    secret: &Secret,
    identity: &naming::AgentIdentity,
    uid: &str,
    env: &BTreeMap<String, String>,
) -> Result<(), ProviderError> {
    if secret.immutable != Some(true)
        || !secret
            .owner_references()
            .iter()
            .any(|owner| owner.uid == uid && owner.kind == "Pod")
        || secret
            .annotations()
            .get(naming::ANNOTATION_PUBKEY_FULL)
            .map(String::as_str)
            != Some(identity.pubkey_hex())
    {
        return Err(ProviderError::refused(
            ErrorCode::CredentialsMismatch,
            "Sandbox credentials belong to a different Pod or agent",
        ));
    }
    let saved_relay = secret
        .data
        .as_ref()
        .and_then(|data| data.get("BUZZ_RELAY_URL"))
        .map(|value| value.0.as_slice());
    if saved_relay != env.get("BUZZ_RELAY_URL").map(|value| value.as_bytes()) {
        return Err(ProviderError::refused(
            ErrorCode::RelayDiffers,
            "Sandbox belongs to a different relay; use a new thread or explicit recovery",
        ));
    }
    Ok(())
}

async fn wait_for_start(
    pods: &Api<Pod>,
    sandbox: &DynamicObject,
    name: &str,
    uid: &str,
) -> Result<String, ProviderError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let child = pods
            .get_opt(name)
            .await
            .map_err(|e| format!("observe Sandbox startup: {e}"))?
            .ok_or_else(|| {
                ProviderError::refused(
                    ErrorCode::PodLost,
                    "Sandbox lost its Pod during startup; explicit recovery is required",
                )
            })?;
        if child.metadata.uid.as_deref() != Some(uid)
            || !owned_pod(&child, sandbox)
            || child.metadata.deletion_timestamp.is_some()
        {
            return Err(ProviderError::refused(
                ErrorCode::PodReplaced,
                "Sandbox Pod changed during startup; explicit recovery is required",
            ));
        }
        let state = child
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|containers| containers.iter().find(|c| c.name == "agent"))
            .and_then(|c| c.state.as_ref());
        if state.is_some_and(|s| s.terminated.is_some()) {
            return Err(ProviderError::refused(
                ErrorCode::PodEnded,
                "Sandbox process ended during startup; inspect its retained logs",
            ));
        }
        if state.is_some_and(|s| s.running.is_some()) {
            return Ok(name.into());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(
                "Sandbox startup timed out; inspect scheduling and image status before retrying"
                    .into(),
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
