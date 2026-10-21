//! `provider_config` parsing and the `info` config schema
//! (spec §`provider_config` v1 fields, `docs/remote-agents.md:1384-1389`).
//!
//! Nine scalar fields and optional structured Pod controls. `image` is required
//! at parse time (the schema offers the published sprig image as a prefill
//! default — §Image).
//! No credential field exists, by I2: cluster auth comes from ambient
//! kubeconfig resolution and nothing else (`:196-198`).

use crate::image::{self, ImageRef};
use k8s_openapi::api::core::v1::Toleration;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Resource requests and limits (§Pod shape: 1cpu/2Gi → 2cpu/4Gi, all four
/// configurable — `cargo build` in an agent workspace makes 500m/1Gi
/// unrealistic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resources {
    pub cpu_request: String,
    pub memory_request: String,
    pub cpu_limit: String,
    pub memory_limit: String,
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            cpu_request: "1".into(),
            memory_request: "2Gi".into(),
            cpu_limit: "2".into(),
            memory_limit: "4Gi".into(),
        }
    }
}

/// Default inactivity budget: the I5 opt-in (§Auto-Stop). The config field and
/// `BUZZ_ACP_EXIT_AFTER_INACTIVITY` are one knob, not two.
pub const DEFAULT_INACTIVITY_SECONDS: u64 = 7200;

/// Default `image` schema prefill: the published sprig image, in tag+digest
/// form so the tag stays human-traceable to its git SHA while the digest does
/// the pinning (§Image — tag-only refs are rejected; `image::parse` drops the
/// tag on normalization). This is a UI prefill, not a baked fallback: `image`
/// stays required, an empty value still fails closed, and the value always
/// arrives explicitly in `provider_config`, so create-intent fingerprints are
/// unaffected by provider upgrades.
pub const DEFAULT_IMAGE: &str = "ghcr.io/block/buzz-sprig:sha-6530b58@sha256:17facfc7608d8ddb33bc056c9aaba1098f4ef6abe5655702fbfd7584d1f74d76";

/// Fixed nonzero UID/GID for the agent container (§Pod shape hardening).
pub const RUN_AS_UID: i64 = 10001;
pub const RUN_AS_GID: i64 = 10001;

/// Writable workspace root; also `HOME` and the harness's cwd
/// (§Working directory).
pub const WORKSPACE_PATH: &str = "/home/agent";

/// Declared shutdown budget, including the 120s checkpoint timeout. The up to
/// 60s wait for a running background turn checkpoint counts from the start of
/// the stop path, so it overlaps the drains instead of adding to them.
/// Child reaping is variable; this is operational margin, not a proven bound
/// on the harness's whole shutdown path (§K8s Grace).
pub const TERMINATION_GRACE_SECONDS: i64 = 300;

/// `buzz-acp`'s stop checkpoint timeout (`checkpoint::save`).
const CHECKPOINT_TIMEOUT_SECONDS: i64 = 120;

/// Known stop-path segments at the desktop's default parallelism of 10
/// (§Stop): wake-task drain and awakened-pool reap, prompt drain and slot
/// reap (5s post-SIGKILL wait per slot each), presence `offline` (2s), and
/// relay close (5s).
const STOP_TAIL_SECONDS: i64 = (30 + 5 * 10) + (30 + 5 * 10) + 2 + 5;

const _: () = assert!(TERMINATION_GRACE_SECONDS >= CHECKPOINT_TIMEOUT_SECONDS + STOP_TAIL_SECONDS);

/// The only restart policy v1 ships. `OnFailure` is double-gated on the
/// harness exit-code contract *and* a crash-loop classification row the state
/// machine does not have (`:1121-1139`); until both land the provider refuses
/// the combination rather than shipping against an undefended convention.
pub const RESTART_POLICY: &str = "Never";

/// Optional placement and resource bounds for a bare Pod. Defaults preserve
/// the original Pod shape; configured fields participate in create intent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PodOptions {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    /// Exact node labels required for scheduling.
    pub node_selector: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Explicit taints this workload may tolerate.
    pub tolerations: Vec<TolerationSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional emptyDir size limit, in Kubernetes quantity syntax.
    pub workspace_size_limit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Scheduler reservation for writable storage and container logs.
    pub ephemeral_storage_request: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Eviction bound for aggregate container ephemeral storage.
    pub ephemeral_storage_limit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Positive absolute Pod lifetime, including startup.
    pub active_deadline_seconds: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    /// Expose the server-assigned Pod UID to a bounded owner setup gate.
    pub setup_pending: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    /// Mount a short-lived ServiceAccount token so an orchestrating agent can
    /// claim sub-agent Sandboxes (§Sub-agents). Off by default.
    pub enable_subagents: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional priority class (a DNS subdomain name), also usable by
    /// namespace-scoped resource quotas.
    pub priority_class_name: Option<String>,
}

impl PodOptions {
    /// Preserve original intent serialization when no options are set.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// A Pod toleration with Kubernetes field names. k8s-openapi's `Toleration`
/// ignores unknown keys, so a misspelled `effect` would tolerate every effect
/// and a misspelled `tolerationSeconds` would make a NoExecute toleration
/// unbounded. This type refuses both before the Pod is built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TolerationSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<TolerationOperator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<TaintEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toleration_seconds: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TolerationOperator {
    Exists,
    Equal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaintEffect {
    NoSchedule,
    PreferNoSchedule,
    NoExecute,
}

impl From<&TolerationSpec> for Toleration {
    fn from(spec: &TolerationSpec) -> Self {
        Self {
            key: spec.key.clone(),
            operator: spec.operator.map(|operator| {
                match operator {
                    TolerationOperator::Exists => "Exists",
                    TolerationOperator::Equal => "Equal",
                }
                .to_string()
            }),
            value: spec.value.clone(),
            effect: spec.effect.map(|effect| {
                match effect {
                    TaintEffect::NoSchedule => "NoSchedule",
                    TaintEffect::PreferNoSchedule => "PreferNoSchedule",
                    TaintEffect::NoExecute => "NoExecute",
                }
                .to_string()
            }),
            toleration_seconds: spec.toleration_seconds,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    /// kubeconfig context; `None` uses the current context.
    pub context: Option<String>,
    pub namespace: String,
    pub image: ImageRef,
    pub resources: Resources,
    /// `None` when `inactivity_seconds` was 0 — refused in v1, see [`parse`].
    pub inactivity_seconds: Option<u64>,
    pub service_account: Option<String>,
    /// Optional Pod placement, storage, and lifetime controls.
    pub pod_options: PodOptions,
}

/// Read an optional non-empty string field. Rejects non-string scalars rather
/// than stringifying them, so a mistyped field is named at the boundary.
fn optional_string(cfg: &serde_json::Value, field: &str) -> Result<Option<String>, String> {
    match cfg.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(other) => Err(format!(
            "provider_config.{field} must be a string, got {other}"
        )),
    }
}

/// Read an optional unsigned integer. The desktop's form omits blank numeric
/// fields rather than sending `""`, but a hand-crafted payload may send a
/// numeric string — accept both, refuse anything else.
fn optional_u64(cfg: &serde_json::Value, field: &str) -> Result<Option<u64>, String> {
    match cfg.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| {
            format!("provider_config.{field} must be a non-negative integer, got {n}")
        }),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().map(Some).map_err(|_| {
            format!("provider_config.{field} must be a non-negative integer, got {s:?}")
        }),
        Some(other) => Err(format!(
            "provider_config.{field} must be a non-negative integer, got {other}"
        )),
    }
}

/// An RFC 1123 subdomain (≤253 chars of dot-separated labels), the name
/// format of cluster objects such as PriorityClasses.
fn valid_dns_subdomain(name: &str) -> bool {
    name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && label.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// A Kubernetes namespace name: RFC 1123 label, ≤63 chars. Validated here so a
/// typo fails with a named field instead of an apiserver rejection partway
/// through a deploy.
fn valid_namespace(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn parse(cfg: &serde_json::Value) -> Result<ProviderConfig, String> {
    if !cfg.is_object() && !cfg.is_null() {
        return Err("provider_config must be a JSON object".to_string());
    }

    let namespace = optional_string(cfg, "namespace")?.ok_or_else(|| {
        "provider_config.namespace is required: the info schema supplies a \
         generated default, so an empty value means the form was cleared"
            .to_string()
    })?;
    if !valid_namespace(&namespace) {
        return Err(format!(
            "provider_config.namespace {namespace:?} is not a valid Kubernetes \
             namespace (lowercase alphanumerics and '-', ≤63 characters)"
        ));
    }

    let image = image::parse(optional_string(cfg, "image")?.unwrap_or_default().as_str())?;

    let defaults = Resources::default();
    let resources = Resources {
        cpu_request: optional_string(cfg, "cpu_request")?.unwrap_or(defaults.cpu_request),
        memory_request: optional_string(cfg, "memory_request")?.unwrap_or(defaults.memory_request),
        cpu_limit: optional_string(cfg, "cpu_limit")?.unwrap_or(defaults.cpu_limit),
        memory_limit: optional_string(cfg, "memory_limit")?.unwrap_or(defaults.memory_limit),
    };

    // `inactivity_seconds: 0` is a legal, blessed value in the spec (§Auto-Stop)
    // meaning "no auto-stop" — but it selects `restartPolicy: OnFailure`, which
    // §Pod shape forbids until the harness exit-code contract is pinned AND the
    // state machine gains a crash-loop row. Refusing the *combination* is what
    // the spec asks for; silently downgrading to `Never` would ship an
    // indefinite agent that dies on its first crash.
    let inactivity_seconds = match optional_u64(cfg, "inactivity_seconds")? {
        None => Some(DEFAULT_INACTIVITY_SECONDS),
        Some(0) => {
            return Err(
                "provider_config.inactivity_seconds: 0 (indefinite lifetime) is not \
                 supported in this version: it requires restartPolicy OnFailure, \
                 which is gated on the harness exit-code contract. Set a positive \
                 number of seconds."
                    .to_string(),
            )
        }
        Some(n) => Some(n),
    };

    let mut pod_options: PodOptions = match cfg.get("pod_options") {
        None => PodOptions::default(),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| format!("provider_config.pod_options: {e}"))?,
    };
    if let Some(name) = pod_options.priority_class_name.as_mut() {
        *name = name.trim().to_string();
        if !valid_dns_subdomain(name) {
            return Err(
                "provider_config.pod_options.priority_class_name must be a PriorityClass name \
                 (DNS subdomain: lowercase letters, digits, '-' and '.')"
                    .into(),
            );
        }
    }
    if pod_options.active_deadline_seconds.is_some_and(|n| n <= 0) {
        return Err("provider_config.pod_options.active_deadline_seconds must be positive".into());
    }
    for (field, value) in [
        ("workspace_size_limit", &pod_options.workspace_size_limit),
        (
            "ephemeral_storage_request",
            &pod_options.ephemeral_storage_request,
        ),
        (
            "ephemeral_storage_limit",
            &pod_options.ephemeral_storage_limit,
        ),
    ] {
        if value
            .as_deref()
            .is_some_and(|value| !valid_storage_quantity(value))
        {
            return Err(format!(
                "provider_config.pod_options.{field} must be a non-negative Kubernetes quantity"
            ));
        }
    }
    // A zero storage limit evicts the Pod as soon as it writes anything.
    for (field, value) in [
        ("workspace_size_limit", &pod_options.workspace_size_limit),
        (
            "ephemeral_storage_limit",
            &pod_options.ephemeral_storage_limit,
        ),
    ] {
        if value.as_deref().is_some_and(zero_quantity) {
            return Err(format!(
                "provider_config.pod_options.{field} must be greater than zero"
            ));
        }
    }

    let service_account = optional_string(cfg, "service_account")?;
    // Without a named ServiceAccount the token would be minted for the
    // namespace's `default` account, which the deployment does not scope.
    if pod_options.enable_subagents && service_account.is_none() {
        return Err(
            "provider_config.pod_options.enable_subagents requires provider_config.service_account: \
             the sub-agent token is minted for that ServiceAccount, never `default`"
                .into(),
        );
    }

    Ok(ProviderConfig {
        context: optional_string(cfg, "context")?,
        namespace,
        image,
        resources,
        inactivity_seconds,
        service_account,
        pod_options,
    })
}

/// A fresh `buzz-agents-<rand6>` namespace default.
///
/// Computed per `info` call, which is how "random default" is satisfied with
/// zero UI changes: the schema's `default` prefills the form (§K8s Namespace).
pub fn generated_namespace() -> String {
    use rand::RngExt;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let suffix: String = (0..6)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect();
    format!("buzz-agents-{suffix}")
}

/// The `config_schema` returned by `info`. Drives the UI form:
/// `properties[*].default` prefill, scalar coercion, `required` gating
/// (`:407-411`).
pub fn config_schema() -> serde_json::Value {
    let defaults = Resources::default();
    serde_json::json!({
        "type": "object",
        "properties": {
            "context": {
                "type": "string",
                "title": "Kubeconfig context",
                "description": "Context from your kubeconfig. Leave empty to use the current context."
            },
            "namespace": {
                "type": "string",
                "title": "Namespace",
                "description": "Created if it does not exist.",
                "default": generated_namespace()
            },
            "image": {
                "type": "string",
                "title": "Agent image",
                "description": "Digest-pinned image containing the buzz-acp runtime ABI, e.g. ghcr.io/block/buzz-sprig@sha256:<digest>. Tags alone are not accepted: this pod holds the agent's private key.",
                "default": DEFAULT_IMAGE
            },
            "cpu_request": {
                "type": "string", "title": "CPU request", "default": defaults.cpu_request
            },
            "memory_request": {
                "type": "string", "title": "Memory request", "default": defaults.memory_request
            },
            "cpu_limit": {
                "type": "string", "title": "CPU limit", "default": defaults.cpu_limit
            },
            "memory_limit": {
                "type": "string", "title": "Memory limit", "default": defaults.memory_limit
            },
            "inactivity_seconds": {
                "type": "number",
                "title": "Stop after inactivity (seconds)",
                "description": "The agent exits after this long with no work, and can be started again at any time.",
                "default": DEFAULT_INACTIVITY_SECONDS
            },
            "pod_options": {
                "type": "object",
                "title": "Pod placement and bounds",
                "description": "Optional structured provider configuration; omit to preserve the original Pod shape.",
                "additionalProperties": false,
                "properties": {
                    "node_selector": {"type": "object", "additionalProperties": {"type": "string"}},
                    "tolerations": {"type": "array", "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "key": {"type": "string"},
                            "operator": {"type": "string", "enum": ["Exists", "Equal"]},
                            "value": {"type": "string"},
                            "effect": {"type": "string", "enum": ["NoSchedule", "PreferNoSchedule", "NoExecute"]},
                            "tolerationSeconds": {"type": "integer"}
                        }
                    }},
                    "workspace_size_limit": {"type": "string", "minLength": 1},
                    "ephemeral_storage_request": {"type": "string", "minLength": 1},
                    "ephemeral_storage_limit": {"type": "string", "minLength": 1},
                    "active_deadline_seconds": {"type": "integer", "minimum": 1},
                    "setup_pending": {"type": "boolean", "default": false},
                    "enable_subagents": {"type": "boolean", "default": false},
                    "priority_class_name": {"type": "string", "minLength": 1}
                }
            },
            "service_account": {
                "type": "string",
                "title": "Service account",
                "description": "Scheduling/RBAC identity only. No API token is mounted."
            },
            "sandbox": {
                "oneOf": [
                    {"const": false},
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["channel_id", "thread_root"],
                        "properties": {
                            "channel_id": {
                                "type": "string",
                                "format": "uuid",
                                "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
                            },
                            "thread_root": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                            "recovery": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["mode"],
                                "properties": {
                                    "mode": {"type": "string", "enum": ["checkpoint", "fresh"]}
                                }
                            }
                        }
                    }
                ],
                "x-buzz-ui-type": "boolean",
                "default": false,
                "title": "Workspace per thread",
                "description": "Use Agent Sandbox. Mention the agent in a channel to create or reuse that thread's workspace. Requires an absolute Pod deadline and BUZZ_CHECKPOINT_BUCKET/BUZZ_CHECKPOINT_PREFIX (or BUZZ_SANDBOX_CHECKPOINT=disabled) in the agent environment. Desktop stores a boolean setting and binds the mentioned thread at launch; deploy accepts only {\"channel_id\": \"<UUID>\", \"thread_root\": \"<event ID>\"} and refuses true. false or absent disables Sandbox."
            },
            "identity_policy": {
                "type": "object",
                "description": "Optional approved agent and owner public keys, cryptographically checked before cluster access. Also enables local --check-identity preflight.",
                "additionalProperties": false,
                "required": ["agent_pubkey", "owner_pubkey"],
                "properties": {
                    "agent_pubkey": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                    "owner_pubkey": {"type": "string", "pattern": "^[0-9a-f]{64}$"}
                }
            }
        },
        "required": ["namespace", "image"]
    })
}

// Quantity is only a serialized string in k8s-openapi. Check Kubernetes' numeric
// grammar and suffixes before creating resources; storage amounts cannot be negative.
fn valid_storage_quantity(value: &str) -> bool {
    let value = value.strip_prefix('+').unwrap_or(value);
    let end = value
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(value.len());
    let (number, suffix) = value.split_at(end);
    if !number.bytes().any(|b| b.is_ascii_digit())
        || number.bytes().filter(|b| *b == b'.').count() > 1
    {
        return false;
    }
    matches!(
        suffix,
        "" | "n"
            | "u"
            | "m"
            | "k"
            | "M"
            | "G"
            | "T"
            | "P"
            | "E"
            | "Ki"
            | "Mi"
            | "Gi"
            | "Ti"
            | "Pi"
            | "Ei"
    ) || suffix
        .strip_prefix(['e', 'E'])
        .is_some_and(|exponent| exponent.parse::<i64>().is_ok())
}

/// Whether a quantity already accepted by [`valid_storage_quantity`] is zero.
/// The suffix only scales the number, so the digits alone decide.
fn zero_quantity(value: &str) -> bool {
    value
        .trim_start_matches('+')
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .all(|c| c == '0' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_ref() -> String {
        format!("ghcr.io/block/buzz-sprig@sha256:{}", "a".repeat(64))
    }

    fn minimal() -> serde_json::Value {
        serde_json::json!({"namespace": "buzz-agents-abc123", "image": digest_ref()})
    }

    #[test]
    fn storage_quantities_are_validated_before_deployment() {
        for field in [
            "workspace_size_limit",
            "ephemeral_storage_request",
            "ephemeral_storage_limit",
        ] {
            for value in [
                "50Gi", "1.5Gi", ".5G", "1.", "+10M", "1e-3", "1E+6", "1m", "1n", "1u", "1Ei",
            ] {
                let mut cfg = minimal();
                cfg["pod_options"] = serde_json::json!({field: value});
                assert!(parse(&cfg).is_ok(), "{field}={value}");
            }
            for value in [
                "",
                "abc",
                "1GB",
                "1K",
                "-1Gi",
                " 1Gi",
                "1Gi ",
                "1.2.3Gi",
                "1e",
                "1e+",
                "1e1.2",
                "1e9223372036854775808",
                "NaN",
                "+",
                ".",
            ] {
                let mut cfg = minimal();
                cfg["pod_options"] = serde_json::json!({field: value});
                let error = parse(&cfg).unwrap_err();
                assert!(error.contains(field), "{field}={value}: {error}");
            }
        }
    }

    #[test]
    fn zero_is_a_valid_storage_request_but_not_a_limit() {
        for value in ["0", "+0", "0Gi", "0.0", ".0", "0e3"] {
            let mut cfg = minimal();
            cfg["pod_options"] = serde_json::json!({"ephemeral_storage_request": value});
            assert!(parse(&cfg).is_ok(), "request {value}");
            for field in ["workspace_size_limit", "ephemeral_storage_limit"] {
                let mut cfg = minimal();
                cfg["pod_options"] = serde_json::json!({field: value});
                let error = parse(&cfg).unwrap_err();
                assert!(error.contains(field), "{field}={value}: {error}");
                assert!(
                    error.contains("greater than zero"),
                    "{field}={value}: {error}"
                );
            }
        }
        let mut cfg = minimal();
        cfg["pod_options"] = serde_json::json!({"ephemeral_storage_limit": "0.5"});
        assert!(parse(&cfg).is_ok());
    }

    #[test]
    fn tolerations_refuse_misspelled_or_unknown_values() {
        for bad in [
            serde_json::json!({"key": "gpu", "operator": "Exists", "efect": "NoSchedule"}),
            serde_json::json!({"key": "gpu", "operator": "Exists", "effect": "NoExecute",
                "tolerationSecond": 60}),
            serde_json::json!({"key": "gpu", "operator": "Exists", "effect": "NoExecute",
                "toleration_seconds": 60}),
            serde_json::json!({"key": "gpu", "operater": "Exists"}),
            serde_json::json!({"key": "gpu", "operator": "exists"}),
            serde_json::json!({"key": "gpu", "operator": "Exists", "effect": "noexecute"}),
        ] {
            let mut cfg = minimal();
            cfg["pod_options"] = serde_json::json!({"tolerations": [bad.clone()]});
            let error = parse(&cfg).unwrap_err();
            assert!(error.contains("pod_options"), "{bad}: {error}");
        }

        let mut cfg = minimal();
        cfg["pod_options"] = serde_json::json!({"tolerations": [{
            "key": "gpu", "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 60
        }]});
        let parsed = parse(&cfg).unwrap();
        let toleration = Toleration::from(&parsed.pod_options.tolerations[0]);
        assert_eq!(toleration.effect.as_deref(), Some("NoExecute"));
        assert_eq!(toleration.operator.as_deref(), Some("Exists"));
        assert_eq!(toleration.toleration_seconds, Some(60));
        assert_eq!(
            serde_json::to_value(&parsed.pod_options).unwrap()["tolerations"],
            cfg["pod_options"]["tolerations"]
        );
    }

    #[test]
    fn schema_closes_toleration_objects() {
        let schema = config_schema();
        let item = &schema["properties"]["pod_options"]["properties"]["tolerations"]["items"];
        assert_eq!(item["additionalProperties"], false);
        let mut keys: Vec<&str> = item["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["effect", "key", "operator", "tolerationSeconds", "value"]
        );
    }

    #[test]
    fn refuses_invalid_pod_options() {
        for bad in [
            serde_json::json!({"active_deadline_seconds": 0}),
            serde_json::json!({"active_deadline_seconds": -1}),
            serde_json::json!({"workspace_size_limit": " "}),
            serde_json::json!({"unknown": true}),
            serde_json::json!({"node_selector": "amd64"}),
            serde_json::json!({"priority_class_name": ""}),
            serde_json::json!({"priority_class_name": "  "}),
            serde_json::json!({"priority_class_name": "Agent-Session"}),
            serde_json::json!({"priority_class_name": "agent_session"}),
            serde_json::json!({"priority_class_name": "agent..session"}),
            serde_json::json!({"priority_class_name": "-agent"}),
            serde_json::json!({"priority_class_name": "a".repeat(254)}),
            serde_json::json!({"priority_class_name": 5}),
            // No service account: the token would belong to `default`.
            serde_json::json!({"enable_subagents": true}),
        ] {
            let mut cfg = minimal();
            cfg["pod_options"] = bad.clone();
            assert!(parse(&cfg).unwrap_err().contains("pod_options"), "{bad}");
        }
        assert!(parse(&minimal()).unwrap().pod_options.is_default());
    }

    #[test]
    fn accepts_subagents_with_a_service_account_and_a_trimmed_priority_class() {
        let mut cfg = minimal();
        cfg["service_account"] = " agent-sa ".into();
        cfg["pod_options"] = serde_json::json!({
            "enable_subagents": true,
            "priority_class_name": " buzz-agent.session-1 ",
        });
        let parsed = parse(&cfg).unwrap();
        assert_eq!(parsed.service_account.as_deref(), Some("agent-sa"));
        assert!(parsed.pod_options.enable_subagents);
        assert_eq!(
            parsed.pod_options.priority_class_name.as_deref(),
            Some("buzz-agent.session-1")
        );
        cfg["pod_options"]["priority_class_name"] = "a".repeat(253).into();
        assert!(parse(&cfg).is_ok());
        cfg["service_account"] = " ".into();
        assert!(parse(&cfg).unwrap_err().contains("enable_subagents"));
    }

    #[test]
    fn applies_spec_defaults() {
        let c = parse(&minimal()).unwrap();
        assert_eq!(c.resources, Resources::default());
        assert_eq!(c.resources.cpu_request, "1");
        assert_eq!(c.resources.memory_request, "2Gi");
        assert_eq!(c.resources.cpu_limit, "2");
        assert_eq!(c.resources.memory_limit, "4Gi");
        assert_eq!(c.inactivity_seconds, Some(DEFAULT_INACTIVITY_SECONDS));
        assert_eq!(c.context, None);
        assert_eq!(c.service_account, None);
    }

    #[test]
    fn all_four_resources_are_configurable() {
        let mut cfg = minimal();
        cfg["cpu_request"] = "500m".into();
        cfg["memory_request"] = "1Gi".into();
        cfg["cpu_limit"] = "4".into();
        cfg["memory_limit"] = "8Gi".into();
        let c = parse(&cfg).unwrap();
        assert_eq!(
            c.resources,
            Resources {
                cpu_request: "500m".into(),
                memory_request: "1Gi".into(),
                cpu_limit: "4".into(),
                memory_limit: "8Gi".into(),
            }
        );
    }

    /// The desktop's form omits blank numeric fields; a hand-crafted payload
    /// may send a numeric string. Both must mean the same thing.
    #[test]
    fn inactivity_accepts_number_string_and_omission() {
        let mut cfg = minimal();
        cfg["inactivity_seconds"] = serde_json::json!(300);
        assert_eq!(parse(&cfg).unwrap().inactivity_seconds, Some(300));

        cfg["inactivity_seconds"] = serde_json::json!("300");
        assert_eq!(parse(&cfg).unwrap().inactivity_seconds, Some(300));

        cfg["inactivity_seconds"] = serde_json::json!("");
        assert_eq!(
            parse(&cfg).unwrap().inactivity_seconds,
            Some(DEFAULT_INACTIVITY_SECONDS)
        );
    }

    /// Indefinite lifetime selects `OnFailure`, which is gated. Refuse rather
    /// than silently downgrade — a downgraded agent dies on its first crash
    /// while the user believes they asked for indefinite.
    #[test]
    fn refuses_indefinite_lifetime() {
        let mut cfg = minimal();
        cfg["inactivity_seconds"] = serde_json::json!(0);
        let err = parse(&cfg).unwrap_err();
        assert!(err.contains("inactivity_seconds"), "got: {err}");
        assert!(
            err.contains("OnFailure"),
            "error should name the gate: {err}"
        );
    }

    #[test]
    fn rejects_negative_and_non_numeric_inactivity() {
        for bad in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("soon"),
            serde_json::json!(true),
        ] {
            let mut cfg = minimal();
            cfg["inactivity_seconds"] = bad.clone();
            assert!(parse(&cfg).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn image_is_required_and_must_be_digest_pinned() {
        let mut cfg = minimal();
        cfg.as_object_mut().unwrap().remove("image");
        assert!(parse(&cfg).unwrap_err().contains("provider_config.image"));

        cfg["image"] = "ghcr.io/block/buzz-sprig:latest".into();
        assert!(parse(&cfg).unwrap_err().contains("digest-pinned"));
    }

    #[test]
    fn rejects_invalid_namespace_names() {
        for bad in [
            "",
            "Buzz-Agents",
            "-leading",
            "trailing-",
            "has_underscore",
            &"n".repeat(64),
        ] {
            let mut cfg = minimal();
            cfg["namespace"] = bad.into();
            assert!(parse(&cfg).is_err(), "accepted namespace {bad:?}");
        }
    }

    /// I2 corollary: there is no config path for cluster credentials, so a
    /// caller that tries to supply one gets no effect from it. Asserting the
    /// parsed struct has no such field is the closest a test can get to
    /// "the type makes it impossible".
    #[test]
    fn credential_fields_have_no_effect() {
        let mut cfg = minimal();
        cfg["token"] = "hunter2".into();
        cfg["client_key"] = "hunter2".into();
        let c = parse(&cfg).unwrap();
        let rendered = format!("{c:?}");
        assert!(
            !rendered.contains("hunter2"),
            "config absorbed a credential: {rendered}"
        );
    }

    #[test]
    fn mistyped_string_fields_are_named() {
        let mut cfg = minimal();
        cfg["namespace"] = serde_json::json!(42);
        assert!(parse(&cfg)
            .unwrap_err()
            .contains("provider_config.namespace"));
    }

    #[test]
    fn generated_namespaces_are_fresh_and_valid() {
        let a = generated_namespace();
        let b = generated_namespace();
        assert_ne!(a, b, "namespace default is not random");
        assert!(valid_namespace(&a), "{a} is not a valid namespace");
        assert!(a.starts_with("buzz-agents-"));
        assert_eq!(a.len(), "buzz-agents-".len() + 6);
    }

    /// The schema's own namespace default must be a value the parser accepts —
    /// otherwise the UI prefills a form that fails on submit.
    #[test]
    fn schema_default_namespace_round_trips_through_parse() {
        let schema = config_schema();
        let default = schema["properties"]["namespace"]["default"]
            .as_str()
            .unwrap();
        let cfg = serde_json::json!({"namespace": default, "image": digest_ref()});
        assert_eq!(parse(&cfg).unwrap().namespace, default);
    }

    /// Same guarantee for the image prefill: the schema's default must be a
    /// value `image::parse` accepts, or the UI prefills a form that fails on
    /// submit. Its tag+digest form normalizes to the tagless canonical form.
    #[test]
    fn schema_default_image_round_trips_through_parse() {
        let schema = config_schema();
        let default = schema["properties"]["image"]["default"].as_str().unwrap();
        assert_eq!(default, DEFAULT_IMAGE);
        let cfg = serde_json::json!({"namespace": "buzz-agents-abc123", "image": default});
        let parsed = parse(&cfg).unwrap();
        assert_eq!(
            parsed.image.as_str(),
            "ghcr.io/block/buzz-sprig@sha256:17facfc7608d8ddb33bc056c9aaba1098f4ef6abe5655702fbfd7584d1f74d76"
        );
    }

    /// Eleven fields (§`provider_config` v1 fields). The cap is 20; the
    /// count is pinned so a field added without a spec change is caught here.
    #[test]
    fn schema_declares_scalar_fields_and_optional_pod_controls() {
        let schema = config_schema();
        let props = schema["properties"].as_object().unwrap();
        let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "context",
                "cpu_limit",
                "cpu_request",
                "identity_policy",
                "image",
                "inactivity_seconds",
                "memory_limit",
                "memory_request",
                "namespace",
                "pod_options",
                "sandbox",
                "service_account"
            ]
        );
        assert_eq!(
            schema["required"],
            serde_json::json!(["namespace", "image"])
        );
    }

    /// I2's key lint rejects any field whose word-split contains
    /// secret|password|token|key|credential. A schema field tripping it would
    /// make every deploy fail validation desktop-side (`:185-198`).
    #[test]
    fn no_schema_field_trips_the_i2_key_lint() {
        const BANNED: [&str; 5] = ["secret", "password", "token", "key", "credential"];
        let schema = config_schema();
        for field in schema["properties"].as_object().unwrap().keys() {
            for word in field.split(['_', '-']) {
                assert!(
                    !BANNED.contains(&word),
                    "field {field:?} contains I2-banned word {word:?}"
                );
            }
        }
    }
}
