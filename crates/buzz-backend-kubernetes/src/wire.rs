//! The stdin/stdout JSON protocol (spec §Provider Protocol).
//!
//! One process per operation: one JSON object in, one JSON object out.
//! These types are this provider's view of the contract; the golden fixtures
//! in `tests/fixtures/provider-wire/` are the arbiter shared with the desktop.

use serde::{Deserialize, Serialize};

use crate::sandbox::StopState;
use std::collections::BTreeMap;

/// The wire-contract version this provider speaks (spec §Info).
pub const PROTOCOL_VERSION: u32 = 1;

/// What this provider supports, advertised in `info.ops`. Additive within
/// protocol version 1. Each entry is a request `op`, except `recover`: it
/// advertises that `deploy` accepts `provider_config.sandbox.recovery`. A
/// desktop gates its session end/recover actions on `recover`.
pub const OPS: &[&str] = &["info", "deploy", "stop", "recover"];

/// Request envelope. `op` discriminates; unknown ops are an in-band error.
///
/// `request_id` is deliberately absent from every variant. The desktop sends
/// it, but the exchange is one request and one response per process, so there
/// is nothing to correlate and nothing in the response schema to echo it into
/// (`:387`, `:416` — neither response shape carries it). Serde ignores it on
/// the way in, so a caller that sends it is accepted; typing it would only
/// create a field nothing reads.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    Info,
    Deploy(Box<DeployRequest>),
    /// End an Agent Sandbox session. Same payload as `deploy`; requires an
    /// approved `identity_policy` and `provider_config.sandbox`.
    Stop(Box<DeployRequest>),
}

#[derive(Debug, Deserialize)]
pub struct DeployRequest {
    pub agent: AgentPayload,
    #[serde(default)]
    pub provider_config: serde_json::Value,
}

/// The agent payload (spec §Deploy).
///
/// Only the fields this binding actually consumes are typed. `name` (display
/// name), `model`, `provider`, and `turn_timeout_seconds` are deliberately
/// absent: object names derive from the pubkey, not the display name
/// (`:1150-1152`), the model/provider pair arrives already resolved inside
/// `launch`, and the timeout is ignored upstream — typing any of them would
/// invite a provider-side remap the spec forbids.
#[derive(Debug, Deserialize)]
pub struct AgentPayload {
    pub relay_url: String,
    pub private_key_nsec: String,
    #[serde(default)]
    pub auth_tag: Option<String>,
    #[serde(default)]
    pub respond_to: Option<String>,
    #[serde(default)]
    pub respond_to_allowlist: Option<Vec<String>>,
    /// User env, already merged global < persona < agent by the desktop and
    /// already stripped of reserved keys. Superseded by `launch.env` when
    /// `launch` is present — a provider MUST NOT re-merge it on top
    /// (§Launch data, precedence tier 2).
    #[serde(default)]
    pub env_vars: BTreeMap<String, String>,
    /// The desktop-resolved launch contract. Absent only from a desktop
    /// predating Known Defect 3's fix.
    #[serde(default)]
    pub launch: Option<LaunchBlock>,
}

/// Desktop-resolved launch data (spec §Launch data).
#[derive(Debug, Default, Deserialize)]
pub struct LaunchBlock {
    /// Command *name*, resolved against the image's PATH — never a host path.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Layered env: baked → runtime metadata → definition → global → persona
    /// → agent. Precedence tier 2.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Overridable behavior defaults. Precedence tier 1 — user env beats
    /// these, matching the local spawn.
    #[serde(default)]
    pub policy_env: BTreeMap<String, String>,
    /// Resolved workspace owner (hex). The respond-to gate's one
    /// irreducible input; without it or `auth_tag` the harness cannot match
    /// `!shutdown`.
    #[serde(default)]
    pub owner_pubkey: Option<String>,
}

/// Response envelope. Serialized flat — `{"ok": true, …}` — because the
/// desktop reads `ok`, `error`, and `agent_id` off the top level.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Response {
    Info(InfoResponse),
    Deploy(DeployResponse),
    IdentityPreflight(IdentityPreflightResponse),
    Stop(StopResponse),
    Error(ErrorResponse),
}

#[derive(Debug, Serialize)]
pub struct InfoResponse {
    pub ok: bool,
    pub name: &'static str,
    pub version: &'static str,
    pub protocol_version: u32,
    pub ops: &'static [&'static str],
    pub description: &'static str,
    pub config_schema: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct DeployResponse {
    pub ok: bool,
    pub agent_id: String,
}

/// `--check-identity` success. `preflight` distinguishes it from a deploy
/// response, so a caller can fail closed when an older provider ignored the
/// flag and deployed.
#[derive(Debug, Serialize)]
pub struct IdentityPreflightResponse {
    pub ok: bool,
    pub agent_id: String,
    pub preflight: &'static str,
}

/// `stop` result: `state` is `ending`, `ended`, or `absent`.
#[derive(Debug, Serialize)]
pub struct StopResponse {
    pub ok: bool,
    pub agent_id: String,
    pub state: StopState,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub ok: bool,
    pub error: String,
    /// Present only for a refusal a client can act on. Additive within
    /// protocol version 1: older clients ignore it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
}

/// Stable, machine-readable reason for a refusal (spec §Sandbox error
/// codes). Clients branch on this, never on the `error` text. A code is never
/// renamed or reused; new refusals get new codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The session is a tombstone; only an explicit recovery starts another.
    SessionEnded,
    /// A stop is draining; the session ends once its Pod is gone.
    SessionEnding,
    /// The lifecycle annotation has a value this provider does not know.
    LifecycleUnknown,
    /// The Sandbox is being deleted.
    SessionDeleting,
    /// The Sandbox is not Running and has no lifecycle record.
    SessionNotRunning,
    /// The live session was launched with a different configuration.
    ConfigDiffers,
    /// The Sandbox belongs to a different agent, owner, or thread, or lacks
    /// the management marker or binding version.
    IdentityMismatch,
    /// The session's credentials were bound to a different Pod or agent.
    CredentialsMismatch,
    /// The session was launched against a different relay.
    RelayDiffers,
    /// The session is bound to its first Pod, but its credentials were not
    /// written within this deploy's deadline; a retry resumes the binding.
    BindingIncomplete,
    /// The session is bound to its first Pod without credentials, and the
    /// Pod is too old to resume before the reaper ends the binding.
    BindingExpired,
    /// The bound container started without its credentials.
    CredentialsMissing,
    /// The Pod the session is bound to is gone.
    PodLost,
    /// The controller replaced the Pod the session is bound to.
    PodReplaced,
    /// The bound Pod finished, is deleting, or its process exited.
    PodEnded,
    /// The Pod with the Sandbox's name is not controlled by it.
    ForeignPod,
    /// A different session replaced this Sandbox during the deploy.
    SessionReplaced,
    /// The agent already has a Pod outside a Sandbox session.
    AgentPodExists,
    /// Recovery was requested for a session that has not ended.
    SessionActive,
    /// Checkpoint recovery was requested, but the thread has no session.
    NoEndedSession,
    /// Checkpoint recovery was requested, but the tombstone records no
    /// verified checkpoint.
    NoCheckpoint,
    /// A recovery deleted the tombstone but did not finish; only an explicit
    /// recovery may continue it.
    RecoveryInterrupted,
    /// A recovery in the other mode created this session's replacement and
    /// has not finished binding it.
    RecoveryModeConflict,
    // Session manager refusals (§Relay transport).
    /// The request has no valid owner attestation, or its owner is not a
    /// configured developer.
    UnauthorizedOwner,
    /// The request is not signed by the key it acts for: the payload's agent
    /// key, or the owner key for `enroll`.
    SignerMismatch,
    /// The request's `created_at` is too far from the manager's clock, or the
    /// request has expired.
    StaleRequest,
    /// The manager has already received this request event.
    ReplayedRequest,
    /// The `request_id` is already in use for a different request.
    RequestIdReused,
    /// The manager is at its limit of tracked or running requests.
    ManagerBusy,
    /// The manager does not support the requested operation.
    UnsupportedOp,
    /// The manager does not support the request's envelope version.
    UnsupportedVersion,
    /// Client-side: the manager did not accept the request within 30 s.
    ManagerUnavailable,
    /// Client-side: the manager accepted the request but did not finish it
    /// within the provider's `timeout_seconds`.
    ManagerTimeout,
    /// Client-side: the provider could not connect or authenticate to the
    /// relay, or lost the connection before the manager answered.
    RelayUnavailable,
}

/// A failed operation: the display message and, for a refusal a client can
/// act on, its [`ErrorCode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub code: Option<ErrorCode>,
    pub message: String,
}

impl ProviderError {
    pub fn refused(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            message: message.into(),
        }
    }
}

impl From<String> for ProviderError {
    fn from(message: String) -> Self {
        Self {
            code: None,
            message,
        }
    }
}

impl From<&str> for ProviderError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl Response {
    pub fn error(message: impl Into<String>) -> Self {
        Self::failed(ProviderError::from(message.into()))
    }

    /// The in-band failure for `error`, carrying its code when it has one.
    pub fn failed(error: ProviderError) -> Self {
        Response::Error(ErrorResponse {
            ok: false,
            error: error.message,
            code: error.code,
        })
    }

    /// The provider's self-description (spec §Info). Pure — no cluster
    /// contact — because the desktop calls it to render the config form
    /// before a kubeconfig is known to exist.
    pub fn info() -> Self {
        Response::Info(InfoResponse {
            ok: true,
            name: "kubernetes",
            version: env!("CARGO_PKG_VERSION"),
            protocol_version: PROTOCOL_VERSION,
            ops: OPS,
            description: "Runs agents as pods in a Kubernetes cluster",
            config_schema: crate::config::config_schema(),
        })
    }

    pub fn stopped(agent_id: impl Into<String>, state: StopState) -> Self {
        Response::Stop(StopResponse {
            ok: true,
            agent_id: agent_id.into(),
            state,
        })
    }

    pub fn deployed(agent_id: impl Into<String>) -> Self {
        Response::Deploy(DeployResponse {
            ok: true,
            agent_id: agent_id.into(),
        })
    }

    pub fn identity_checked(agent_id: impl Into<String>) -> Self {
        Response::IdentityPreflight(IdentityPreflightResponse {
            ok: true,
            agent_id: agent_id.into(),
            preflight: "identity",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_info_request() {
        let r: Request = serde_json::from_str(r#"{"op":"info","request_id":"abc"}"#).unwrap();
        assert!(matches!(r, Request::Info));
    }

    /// The desktop sends `request_id` on every call, but the exchange is 1:1
    /// per process — a provider that hard-required it would fail a
    /// conforming-but-minimal caller for no safety gain.
    #[test]
    fn request_id_is_optional() {
        let r: Request = serde_json::from_str(r#"{"op":"info"}"#).unwrap();
        assert!(matches!(r, Request::Info));
    }

    #[test]
    fn parses_stop_with_the_deploy_payload() {
        let r: Request = serde_json::from_str(
            r#"{"op":"stop","agent":{"relay_url":"wss://r","private_key_nsec":"nsec1x"}}"#,
        )
        .unwrap();
        assert!(matches!(r, Request::Stop(_)));
    }

    #[test]
    fn stop_response_serializes_flat() {
        let v = serde_json::to_value(Response::stopped("buzz-agent-abc-123", StopState::Ending))
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"ok": true, "agent_id": "buzz-agent-abc-123", "state": "ending"})
        );
    }

    #[test]
    fn info_advertises_stop_and_recover() {
        let v = serde_json::to_value(Response::info()).unwrap();
        assert_eq!(
            v["ops"],
            serde_json::json!(["info", "deploy", "stop", "recover"])
        );
    }

    #[test]
    fn rejects_unknown_op() {
        assert!(serde_json::from_str::<Request>(r#"{"op":"undeploy"}"#).is_err());
    }

    /// Payload fields this binding does not consume must not break parsing:
    /// the desktop sends `model`, `provider`, `system_prompt` and more, and a
    /// provider that rejected them would break on every real deploy.
    #[test]
    fn ignores_unconsumed_payload_fields() {
        let json = r#"{
            "op":"deploy","request_id":"r1",
            "agent":{
                "name":"a","relay_url":"wss://r","private_key_nsec":"nsec1x",
                "model":"gpt-5","provider":"openai","system_prompt":"hi",
                "turn_timeout_seconds":30,"parallelism":10,
                "agent_command":"goose","agent_args":[]
            },
            "provider_config":{"namespace":"ns"}
        }"#;
        let r: Request = serde_json::from_str(json).unwrap();
        let Request::Deploy(d) = r else {
            panic!("wrong op")
        };
        assert_eq!(d.agent.relay_url, "wss://r");
        assert!(d.agent.launch.is_none());
    }

    #[test]
    fn parses_launch_block() {
        let json = r#"{
            "op":"deploy",
            "agent":{
                "relay_url":"wss://r","private_key_nsec":"nsec1x",
                "launch":{
                    "command":"goose","args":["run","--x"],
                    "env":{"GOOSE_MODEL":"m"},
                    "policy_env":{"GOOSE_MODE":"auto"},
                    "owner_pubkey":"deadbeef"
                }
            }
        }"#;
        let Request::Deploy(d) = serde_json::from_str::<Request>(json).unwrap() else {
            panic!("wrong op")
        };
        let l = d.agent.launch.unwrap();
        assert_eq!(l.command.as_deref(), Some("goose"));
        assert_eq!(l.args, ["run", "--x"]);
        assert_eq!(l.env["GOOSE_MODEL"], "m");
        assert_eq!(l.policy_env["GOOSE_MODE"], "auto");
        assert_eq!(l.owner_pubkey.as_deref(), Some("deadbeef"));
    }

    /// A null `owner_pubkey`/`auth_tag` must parse (the refusal is a policy
    /// decision made later, with a specific message), not fail as a type error.
    #[test]
    fn null_owner_fields_parse() {
        let json = r#"{"op":"deploy","agent":{
            "relay_url":"wss://r","private_key_nsec":"nsec1x","auth_tag":null,
            "launch":{"owner_pubkey":null}
        }}"#;
        let Request::Deploy(d) = serde_json::from_str::<Request>(json).unwrap() else {
            panic!("wrong op")
        };
        assert!(d.agent.auth_tag.is_none());
        assert!(d.agent.launch.unwrap().owner_pubkey.is_none());
    }

    /// The desktop reads `ok`/`error`/`agent_id` off the top level, so the
    /// enum must serialize flat with no variant tag.
    #[test]
    fn responses_serialize_flat() {
        let v = serde_json::to_value(Response::deployed("buzz-agent-abc")).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["agent_id"], "buzz-agent-abc");
        assert!(v.get("Deploy").is_none());
        assert!(v.get("preflight").is_none());

        let v = serde_json::to_value(Response::identity_checked("buzz-agent-abc")).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["agent_id"], "buzz-agent-abc");
        assert_eq!(v["preflight"], "identity");

        let v = serde_json::to_value(Response::error("boom")).unwrap();
        assert_eq!(v, serde_json::json!({"ok": false, "error": "boom"}));
    }

    /// A refusal's code is a stable snake_case string next to `error`; an
    /// uncoded failure omits the field, so protocol-v1 clients see no change.
    #[test]
    fn refusal_codes_serialize_snake_case() {
        let v = serde_json::to_value(Response::failed(ProviderError::refused(
            ErrorCode::SessionNotRunning,
            "not running",
        )))
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"ok": false, "error": "not running", "code": "session_not_running"})
        );
    }
}
