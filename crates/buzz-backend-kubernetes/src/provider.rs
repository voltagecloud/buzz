//! The provider protocol operations (spec §Provider Protocol), shared by the
//! stdin/stdout binary and the in-cluster session manager
//! (`buzz-sandbox-reaper serve`, §Relay transport).

use crate::wire::{self, Request, Response};
use crate::{client, cluster, config, env, identity, naming, reconcile, sandbox};

/// The provider a shared-compute agent resolves to. Refused here as the
/// spec's backstop: a mesh agent runs on the relay's compute, so deploying it
/// as a pod would create a second, contending consumer of the same agent
/// identity (`:214-219`).
const RELAY_MESH_PROVIDER: &str = "relay-mesh";

/// Where an operation gets its cluster client and namespace.
// One `Target` exists per request, so boxing `Managed` would save nothing.
#[allow(clippy::large_enum_variant)]
pub enum Target {
    /// The caller's kubeconfig (`provider_config.context`) and
    /// `provider_config.namespace`, as the desktop runs this provider.
    Kubeconfig,
    /// The in-cluster session manager (§Relay transport): its own client, the
    /// namespace it derived from the verified owner, and the agent and owner
    /// it verified. These replace `provider_config.namespace` and
    /// `identity_policy`, so a request cannot choose them, and only Sandbox
    /// sessions are allowed.
    Managed {
        client: kube::Client,
        namespace: String,
        agent: nostr::PublicKey,
        owner: nostr::PublicKey,
        /// `provider_config` fields the manager sets on every request, such
        /// as `image` and `pod_options`. They replace the request's own.
        defaults: serde_json::Map<String, serde_json::Value>,
        /// Launch environment the manager sets for the owner's developer, such
        /// as the profile and checkpoint location. It replaces the request's
        /// own values for these keys.
        env: std::collections::BTreeMap<String, String>,
    },
}

/// Fields a managed target decides itself, so its defaults cannot set them.
const MANAGED_FIELDS: &[&str] = &["context", "namespace", "identity_policy", "sandbox"];

/// Validate a managed target's `provider_config` defaults: no field the
/// manager derives per request, and a complete configuration once those are
/// added, including a digest-pinned `image`.
pub fn check_managed_defaults(
    defaults: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    if let Some(field) = MANAGED_FIELDS.iter().find(|f| defaults.contains_key(**f)) {
        return Err(format!(
            "the session manager derives provider_config.{field} itself"
        ));
    }
    let mut config = defaults.clone();
    config.insert("namespace".into(), "defaults-check".into());
    config::parse(&serde_json::Value::Object(config)).map(|_| ())
}

/// The refusals a Sandbox `deploy` or `stop` makes before any cluster
/// contact: the `sandbox` scope's shape and the approved `identity_policy`.
/// A provider that forwards the request (`buzz-backend-relay`) runs them
/// first, so a request that would be refused never leaves the machine.
pub fn check_sandbox_request(request: &wire::DeployRequest) -> Result<(), wire::ProviderError> {
    sandbox::Options::parse(&request.provider_config)?
        .ok_or("the session manager starts Sandbox sessions only")?;
    identity::check(request, true)?;
    Ok(())
}

impl Target {
    /// Overwrite what the managed target decides. A no-op for kubeconfig.
    fn confine(&self, request: &mut wire::DeployRequest) -> Result<(), wire::ProviderError> {
        let Target::Managed {
            namespace,
            agent,
            owner,
            defaults,
            env,
            ..
        } = self
        else {
            return Ok(());
        };
        // The user tier (`launch.env`, or `env_vars` without a launch block),
        // so the manager's values replace the request's own.
        match request.agent.launch.as_mut() {
            Some(launch) => launch.env.extend(env.clone()),
            None => request.agent.env_vars.extend(env.clone()),
        }
        let config = request
            .provider_config
            .as_object_mut()
            .ok_or("provider_config must be an object")?;
        config.extend(defaults.clone());
        config.remove("context");
        config.insert("namespace".into(), namespace.as_str().into());
        config.insert(
            "identity_policy".into(),
            serde_json::json!({"agent_pubkey": agent.to_hex(), "owner_pubkey": owner.to_hex()}),
        );
        if sandbox::Options::parse(&request.provider_config)?.is_none() {
            return Err("the session manager starts Sandbox sessions only".into());
        }
        Ok(())
    }

    async fn client(&self, cfg: &config::ProviderConfig) -> Result<kube::Client, String> {
        match self {
            Target::Kubeconfig => client::connect(cfg.context.as_deref()).await,
            Target::Managed { client, .. } => Ok(client.clone()),
        }
    }
}

/// Local identity preflight; never connects to Kubernetes or forwards secrets.
pub fn check_identity(input: &str) -> Response {
    let request: Request = match serde_json::from_str(input) {
        Ok(request) => request,
        Err(_) => return Response::error("invalid identity preflight request"),
    };
    let Request::Deploy(request) = request else {
        return Response::error("identity preflight requires a deploy request");
    };
    match identity::check(&request, true) {
        Ok(identity) => Response::identity_checked(identity.pod_name()),
        Err(error) => Response::error(error),
    }
}

/// Produce the single response for one stdin request, against the caller's
/// kubeconfig. Separated from `main` so the whole dispatch is testable
/// without a process.
pub fn respond(input: &str) -> Response {
    // Parsed as raw JSON first: the relay-mesh refusal MUST see the wire
    // value, and `AgentPayload` deliberately does not carry `provider`.
    let raw: serde_json::Value = match serde_json::from_str(input) {
        Ok(value) => value,
        Err(e) => return Response::error(format!("request is not valid JSON: {e}")),
    };
    match parse(raw) {
        Ok(Request::Info) => Response::info(),
        Ok(request) => match runtime() {
            Ok(runtime) => runtime.block_on(run(request, &Target::Kubeconfig)),
            Err(e) => Response::error(e),
        },
        Err(refusal) => refusal,
    }
}

/// Parse a request from its raw JSON, refusing a relay-mesh agent first.
pub fn parse(raw: serde_json::Value) -> Result<Request, Response> {
    if let Some(refusal) = refuse_relay_mesh(&raw) {
        return Err(Response::error(refusal));
    }
    serde_json::from_value(raw)
        .map_err(|e| Response::error(format!("could not understand the request: {e}")))
}

/// Run one request to its single response.
pub async fn run(request: Request, target: &Target) -> Response {
    match request {
        Request::Info => Response::info(),
        Request::Deploy(mut deploy) => match target.confine(&mut deploy) {
            Ok(()) => match deploy_agent(&deploy, target).await {
                Ok(agent_id) => Response::deployed(agent_id),
                Err(e) => Response::failed(e),
            },
            Err(e) => Response::failed(e),
        },
        Request::Stop(mut stop) => match target.confine(&mut stop) {
            Ok(()) => match stop_session(&stop, target).await {
                Ok((agent_id, state)) => Response::stopped(agent_id, state),
                Err(e) => Response::failed(e),
            },
            Err(e) => Response::failed(e),
        },
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start the runtime: {e}"))
}

/// End one Sandbox session. Refusals (identity, scope) precede any cluster
/// contact, exactly as for `deploy`.
async fn stop_session(
    request: &wire::DeployRequest,
    target: &Target,
) -> Result<(String, sandbox::StopState), wire::ProviderError> {
    let cfg = config::parse(&request.provider_config)?;
    let scope = sandbox::Options::parse(&request.provider_config)?
        .ok_or("stop requires provider_config.sandbox")?;
    let identity = identity::check(request, true)?;
    let owner = request
        .agent
        .launch
        .as_ref()
        .and_then(|launch| launch.owner_pubkey.as_deref())
        .ok_or("Sandbox requires a verified launch owner")?;
    let client = target.client(&cfg).await?;
    sandbox::stop(client, &identity, &cfg.namespace, &scope, owner).await
}

/// Refuse a shared-compute agent, reading the **raw wire value**.
///
/// Trimmed before comparing: the desktop's own layers disagree about padding
/// (`relay_mesh.rs:17` and `effective_config/mod.rs:46` trim; the deploy guard
/// at `agents_deploy.rs:116` did not), and `non_blank` preserves surrounding
/// whitespace on a non-blank value. A backstop that shares its bypass with the
/// layer it backs is not a backstop.
fn refuse_relay_mesh(raw: &serde_json::Value) -> Option<String> {
    let provider = raw.get("agent")?.get("provider")?.as_str()?;
    (provider.trim() == RELAY_MESH_PROVIDER).then(|| {
        "deploy refused: this agent is configured for shared compute \
         (relay-mesh), which runs on the relay rather than in a pod. \
         Switch the agent to a local runtime before deploying it to \
         Kubernetes."
            .to_string()
    })
}

/// Run one deploy to a terminal outcome.
async fn deploy_agent(
    request: &wire::DeployRequest,
    target: &Target,
) -> Result<String, wire::ProviderError> {
    let cfg = config::parse(&request.provider_config)?;
    let sandbox = sandbox::Options::parse(&request.provider_config)?;
    // Identity before any cluster contact: a malformed nsec is a refusal, not
    // a failed connection (§Deploy State Machine step 0).
    let identity = identity::check(request, sandbox.is_some())?;

    // One generation for this operation's first attempt; the reconciler mints
    // its own per attempt and restamps the correlator to match.
    let env = env::build_env(
        &request.agent,
        env::AuthoritativeInputs {
            generation: &naming::new_generation(),
            inactivity_seconds: cfg.inactivity_seconds,
        },
    )?;

    let client = target.client(&cfg).await?;
    if let Some(scope) = sandbox {
        let owner = request
            .agent
            .launch
            .as_ref()
            .and_then(|launch| launch.owner_pubkey.as_deref())
            .ok_or("Sandbox requires a verified launch owner")?;
        return sandbox::deploy(client, &identity, &cfg, &scope, owner, env).await;
    }
    let substrate = cluster::Cluster::new(client, &cfg.namespace);
    Ok(reconcile::deploy(&substrate, &identity, &cfg, env).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_of(response: &Response) -> String {
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["ok"], false, "expected a refusal: {json}");
        json["error"].as_str().unwrap().to_string()
    }

    /// The spec's backstop for the relay-mesh MUST. The desktop refuses first
    /// (`agents_deploy.rs:116`); this is the layer that owes the obligation.
    #[test]
    fn refuses_a_relay_mesh_agent() {
        let request = r#"{"op":"deploy","agent":{
            "relay_url":"wss://r","private_key_nsec":"nsec1x","provider":"relay-mesh"},
            "provider_config":{"namespace":"ns"}}"#;
        assert!(error_of(&respond(request)).contains("relay-mesh"));
    }

    /// Padding must not bypass the backstop. Reachable by construction:
    /// `GlobalConfig.provider` is a bare `Option<String>` with no trim on
    /// write, and `non_blank` rejects whitespace-only while preserving
    /// surrounding whitespace on everything else.
    #[test]
    fn refuses_a_padded_relay_mesh_agent() {
        let request = r#"{"op":"deploy","agent":{
            "relay_url":"wss://r","private_key_nsec":"nsec1x","provider":"  relay-mesh  "},
            "provider_config":{"namespace":"ns"}}"#;
        assert!(error_of(&respond(request)).contains("relay-mesh"));
    }

    /// The refusal must not fire on a normal agent — a guard that refuses
    /// everything passes its own test and ships a provider that deploys
    /// nothing.
    #[test]
    fn does_not_refuse_a_normal_provider() {
        let raw: serde_json::Value =
            serde_json::from_str(r#"{"agent":{"provider":"openai"}}"#).unwrap();
        assert!(refuse_relay_mesh(&raw).is_none());
        // …nor when the field is absent entirely, which is the common case:
        // `AgentPayload` does not carry `provider`.
        let bare: serde_json::Value = serde_json::from_str(r#"{"agent":{}}"#).unwrap();
        assert!(refuse_relay_mesh(&bare).is_none());
    }

    /// Malformed input still produces exactly one conforming response.
    #[test]
    fn malformed_input_is_an_in_band_error() {
        assert!(error_of(&respond("not json")).contains("valid JSON"));
        assert!(error_of(&respond(r#"{"op":"undeploy"}"#)).contains("understand"));
    }

    /// `info` answers without touching a cluster — it is what the desktop
    /// calls to render the config form, before any kubeconfig exists.
    #[test]
    fn info_answers_with_the_protocol_version_and_schema() {
        let json = serde_json::to_value(respond(r#"{"op":"info"}"#)).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["protocol_version"], wire::PROTOCOL_VERSION);
        assert!(json["config_schema"]["properties"]["namespace"].is_object());
        assert_eq!(
            json["ops"],
            serde_json::json!(["info", "deploy", "stop", "recover"])
        );
    }

    /// `stop` is gated like a preflight: no approved identity policy, no
    /// cluster contact.
    #[test]
    fn stop_requires_sandbox_scope_and_identity_policy_before_cluster_contact() {
        use nostr::nips::nip19::ToBech32;
        let agent = nostr::Keys::generate();
        let mut request = serde_json::json!({
            "op": "stop",
            "agent": {"relay_url": "wss://example.invalid",
                "private_key_nsec": agent.secret_key().to_bech32().unwrap()},
            "provider_config": {"namespace": "test",
                "image": format!("example.invalid/pilot@sha256:{}", "a".repeat(64))}
        });
        assert!(error_of(&respond(&request.to_string())).contains("provider_config.sandbox"));
        request["provider_config"]["sandbox"] = serde_json::json!({
            "channel_id": uuid::Uuid::new_v4().to_string(), "thread_root": "a".repeat(64)
        });
        assert!(error_of(&respond(&request.to_string())).contains("identity_policy"));
    }

    #[test]
    fn identity_policy_refusal_precedes_cluster_connection_and_hides_secrets() {
        use nostr::nips::nip19::ToBech32;
        let agent = nostr::Keys::generate();
        let owner = nostr::Keys::generate();
        let request = serde_json::json!({
            "op": "deploy",
            "agent": {
                "relay_url": "wss://example.invalid",
                "private_key_nsec": agent.secret_key().to_bech32().unwrap(),
                "auth_tag": "synthetic-secret-not-an-attestation",
                "launch": {"owner_pubkey": owner.public_key().to_hex()}
            },
            "provider_config": {
                "namespace": "test",
                "image": format!("example.invalid/pilot@sha256:{}", "a".repeat(64)),
                "identity_policy": {"agent_pubkey": agent.public_key().to_hex(),
                                    "owner_pubkey": owner.public_key().to_hex()}
            }
        })
        .to_string();
        let error = error_of(&respond(&request));
        assert_eq!(error, "owner attestation verification failed");
        assert!(!error.contains("synthetic-secret"));
        assert_eq!(error_of(&check_identity(&request)), error);
    }

    #[test]
    fn local_identity_preflight_returns_only_public_pod_identity() {
        use nostr::nips::nip19::ToBech32;
        let agent = nostr::Keys::generate();
        let owner = nostr::Keys::generate();
        let request = serde_json::json!({
            "op": "deploy", "agent": {
                "relay_url": "wss://example.invalid",
                "private_key_nsec": agent.secret_key().to_bech32().unwrap(),
                "auth_tag": buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap(),
                "launch": {"owner_pubkey": owner.public_key().to_hex()}
            },
            "provider_config": {"identity_policy": {
                "agent_pubkey": agent.public_key().to_hex(), "owner_pubkey": owner.public_key().to_hex()
            }}
        }).to_string();
        let response = serde_json::to_value(check_identity(&request)).unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(
            response["agent_id"],
            format!("buzz-agent-{}", &agent.public_key().to_hex()[..12])
        );
        // A deploy response has no `preflight`; callers rely on its presence
        // to detect a provider that ignored the flag and deployed.
        assert_eq!(response["preflight"], "identity");
        assert_eq!(response.as_object().unwrap().len(), 3);
        assert!(error_of(&check_identity(r#"{"op":"info"}"#)).contains("deploy request"));
    }

    fn managed(agent: &nostr::Keys, owner: &nostr::Keys) -> Target {
        Target::Managed {
            client: crate::sandbox_fake::client(&Default::default()),
            namespace: "buzz-agent-staging-dev".into(),
            agent: agent.public_key(),
            owner: owner.public_key(),
            defaults: serde_json::Map::new(),
            env: Default::default(),
        }
    }

    /// The managed target, not the request, decides the namespace and the
    /// approved identity, and allows only Sandbox sessions.
    #[tokio::test]
    async fn managed_target_replaces_namespace_and_identity_policy() {
        let (agent, owner) = (nostr::Keys::generate(), nostr::Keys::generate());
        let stranger = nostr::Keys::generate().public_key().to_hex();
        let mut request: wire::DeployRequest = serde_json::from_value(serde_json::json!({
            "agent": {"relay_url": "wss://r", "private_key_nsec": "nsec1x"},
            "provider_config": {"namespace": "someone-else", "context": "admin",
                "identity_policy": {"agent_pubkey": stranger, "owner_pubkey": stranger},
                "sandbox": {"channel_id": uuid::Uuid::new_v4().to_string(),
                    "thread_root": "a".repeat(64)}}
        }))
        .unwrap();
        let mut unchanged = request.provider_config.clone();
        Target::Kubeconfig.confine(&mut request).unwrap();
        assert_eq!(request.provider_config, unchanged);

        managed(&agent, &owner).confine(&mut request).unwrap();
        let config = &request.provider_config;
        assert_eq!(config["namespace"], "buzz-agent-staging-dev");
        assert!(config.get("context").is_none());
        assert_eq!(
            config["identity_policy"],
            serde_json::json!({"agent_pubkey": agent.public_key().to_hex(),
                "owner_pubkey": owner.public_key().to_hex()})
        );

        unchanged["sandbox"] = serde_json::json!(false);
        request.provider_config = unchanged;
        let error = managed(&agent, &owner).confine(&mut request).unwrap_err();
        assert_eq!(
            error.message,
            "the session manager starts Sandbox sessions only"
        );
    }

    /// The manager's launch environment replaces the request's values for its
    /// keys, in either user tier, and leaves the developer's other choices,
    /// such as the model, effort, and fast mode, as the request sent them.
    #[tokio::test]
    async fn managed_env_replaces_its_keys_and_keeps_the_rest() {
        let (agent, owner) = (nostr::Keys::generate(), nostr::Keys::generate());
        let mut target = managed(&agent, &owner);
        if let Target::Managed { env, .. } = &mut target {
            env.insert("BUZZ_CHECKPOINT_PREFIX".into(), "dev/".into());
            env.insert("BUZZ_DEVELOPER_ID".into(), "dev".into());
        }
        let sandbox = serde_json::json!({"sandbox": {"channel_id": uuid::Uuid::new_v4().to_string(),
            "thread_root": "a".repeat(64)}});
        let mut launched: wire::DeployRequest = serde_json::from_value(serde_json::json!({
            "agent": {"relay_url": "wss://r", "private_key_nsec": "nsec1x",
                "launch": {"command": "claude-agent-acp",
                    "env": {"BUZZ_CHECKPOINT_PREFIX": "someone-else/", "KEEP": "1"},
                    "policy_env": {"ANTHROPIC_MODEL": "claude-opus-5-5",
                        "BUZZ_ACP_EFFORT_LEVEL": "xhigh", "BUZZ_ACP_FAST_MODE": "on"}}},
            "provider_config": sandbox,
        }))
        .unwrap();
        target.confine(&mut launched).unwrap();
        let launch = launched.agent.launch.as_ref().unwrap();
        assert_eq!(launch.env["BUZZ_CHECKPOINT_PREFIX"], "dev/");
        assert_eq!(launch.env["BUZZ_DEVELOPER_ID"], "dev");
        assert_eq!(launch.env["KEEP"], "1");
        assert_eq!(launch.policy_env["ANTHROPIC_MODEL"], "claude-opus-5-5");
        assert_eq!(launch.policy_env["BUZZ_ACP_EFFORT_LEVEL"], "xhigh");
        assert_eq!(launch.policy_env["BUZZ_ACP_FAST_MODE"], "on");
        assert!(launched.agent.env_vars.is_empty());
        // Without a launch block, the legacy user tier carries them.
        let mut plain: wire::DeployRequest = serde_json::from_value(serde_json::json!({
            "agent": {"relay_url": "wss://r", "private_key_nsec": "nsec1x",
                "env_vars": {"BUZZ_DEVELOPER_ID": "someone-else"}},
            "provider_config": sandbox,
        }))
        .unwrap();
        target.confine(&mut plain).unwrap();
        assert_eq!(plain.agent.env_vars["BUZZ_DEVELOPER_ID"], "dev");
    }

    /// The manager's defaults replace the request's fields and may not set a
    /// field the manager derives per request. (The forwarding checks in
    /// `check_sandbox_request` are tested through `buzz-backend-relay`.)
    #[tokio::test]
    async fn managed_defaults_replace_request_fields() {
        let (agent, owner) = (nostr::Keys::generate(), nostr::Keys::generate());
        let image = format!("example.invalid/manager@sha256:{}", "b".repeat(64));
        let defaults = serde_json::json!({"image": image});
        let defaults = defaults.as_object().unwrap().clone();
        check_managed_defaults(&defaults).unwrap();
        let mut target = managed(&agent, &owner);
        if let Target::Managed { defaults: d, .. } = &mut target {
            *d = defaults.clone();
        }
        let mut request: wire::DeployRequest = serde_json::from_value(serde_json::json!({
            "agent": {"relay_url": "wss://r", "private_key_nsec": "nsec1x"},
            "provider_config": {"image": "attacker.invalid/x@sha256:00",
                "sandbox": {"channel_id": uuid::Uuid::new_v4().to_string(),
                    "thread_root": "a".repeat(64)}}
        }))
        .unwrap();
        target.confine(&mut request).unwrap();
        assert_eq!(request.provider_config["image"], image);
        for field in MANAGED_FIELDS {
            let mut bad = defaults.clone();
            bad.insert(field.to_string(), serde_json::json!("x"));
            assert!(check_managed_defaults(&bad).unwrap_err().contains(field));
        }
    }

    /// A managed deploy still runs the identity check against the verified
    /// agent, before any cluster contact.
    #[tokio::test]
    async fn managed_run_refuses_an_agent_key_other_than_the_verified_one() {
        use nostr::nips::nip19::ToBech32;
        let (agent, owner) = (nostr::Keys::generate(), nostr::Keys::generate());
        let other = nostr::Keys::generate();
        let request = parse(serde_json::json!({
            "op": "stop",
            "agent": {"relay_url": "wss://r",
                "private_key_nsec": other.secret_key().to_bech32().unwrap()},
            "provider_config": {"image": format!("example.invalid/p@sha256:{}", "a".repeat(64)),
                "sandbox": {"channel_id": uuid::Uuid::new_v4().to_string(),
                    "thread_root": "a".repeat(64)}}
        }))
        .unwrap();
        let response = run(request, &managed(&agent, &owner)).await;
        assert!(error_of(&response).contains("approved agent identity"));
    }
}
