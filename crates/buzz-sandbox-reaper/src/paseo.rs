//! Per-developer Paseo daemons (`voltage/remote-agents.md` §Paseo): the
//! pairing fields kept only in the narrow Kubernetes Secret that the
//! developer's daemon reads its environment from, and
//! the pairing offer the Paseo apps open.
//!
//! The daemon has less access than the developer's Sandboxes: it reads only
//! this Secret, never the profile, and the Secret holds only the pairing, the
//! developer's registered agents, native assistant tokens and the reserved gateway
//! key. Nothing here logs a value, and no failure label quotes one.

use std::collections::{BTreeMap, BTreeSet};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use curve25519_dalek::montgomery::MontgomeryPoint;
use k8s_openapi::api::core::v1::Secret;
use kube::{
    api::{ListParams, PostParams},
    Api,
};
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::manager::Developer;
use crate::mobile_launch::{self, Registration, CHANNEL_ANNOTATION, DATA_KEY, LABEL};
use buzz_sdk::sharing::policy::ManagedGateway;

/// The namespace every developer's daemon runs in (Terraform `buzz-paseo`).
pub const NAMESPACE: &str = "buzz-paseo";
/// The default Paseo relay endpoint: served next to the Buzz relay, always
/// over TLS. `paseo_relay_endpoint` in the manager config overrides it.
pub const RELAY_ENDPOINT: &str = "buzz.voltage.cloud:443";
/// The Paseo web app that opens a pairing offer.
const APP_BASE_URL: &str = "https://app.paseo.sh";
/// Writes before a concurrent writer's conflict is reported.
const WRITE_ATTEMPTS: usize = 3;
/// Registrations one developer's daemon lists, as the mention listener caps
/// them.
const MAX_AGENTS: usize = 64;
/// Registrations read to build that list. Some are left out (another relay,
/// invalid data), so the read is wider than the list, and still bounded.
const MAX_LISTED: usize = 4 * MAX_AGENTS;
/// Maximum configured daemon identities read during one shared admission.
pub(crate) const MAX_DAEMON_IDENTITIES: usize = 64;
/// The daemon's provider list; only legacy entries include `channel`.
pub const AGENTS_KEY: &str = "BUZZ_PASEO_AGENTS";
/// Developer-supplied fine-grained, selected-repository read-only Git token.
/// This narrow Secret key is explicitly mapped to GITHUB_READ_TOKEN by the pod.
pub const GIT_READ_TOKEN_KEY: &str = "github_read_token";
/// Owner-encrypted daemon-only token change; never part of the Sandbox profile.
pub enum GitReadToken {
    /// Replace the daemon-only token.
    Set(Zeroizing<String>),
    /// Remove the daemon-only token.
    Remove,
}

impl GitReadToken {
    /// Parse only the narrow enrollment object. Omission preserves the token.
    pub fn parse(value: Option<&Value>) -> Result<Option<Self>, Failed> {
        let Some(value) = value else { return Ok(None) };
        let object = value
            .as_object()
            .filter(|object| object.len() == 1 && object.contains_key(GIT_READ_TOKEN_KEY))
            .ok_or(Failed("invalid_git_read_token"))?;
        let token = &object[GIT_READ_TOKEN_KEY];
        if token.is_null() {
            return Ok(Some(Self::Remove));
        }
        let token = token
            .as_str()
            .filter(|token| valid_git_read_token(token))
            .ok_or(Failed("invalid_git_read_token"))?;
        Ok(Some(Self::Set(Zeroizing::new(token.to_owned()))))
    }
}

fn valid_git_read_token(token: &str) -> bool {
    token.starts_with("github_pat_")
        && (12..=4096).contains(&token.len())
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The profile's model tokens the daemon's own agent may use, by the
/// variable it reads. No other profile credential reaches the daemon.
const MODEL_TOKENS: &[(&str, &str)] = &[
    ("claude_oauth_token", "CLAUDE_CODE_OAUTH_TOKEN"),
    ("codex_access_token", "CODEX_ACCESS_TOKEN"),
];
/// Provider IDs the daemon image defines itself; an agent never takes one.
const RESERVED_IDS: &[&str] = &[
    "acp",
    "antigravity",
    "claude",
    "codex",
    "copilot",
    "cursor",
    "hoplite",
    "muse",
    "omp",
    "opencode",
    "pi",
];
/// Longest agent ID and label the manager derives.
const MAX_ID: usize = 32;
const MAX_LABEL: usize = 64;

/// Why generating the pairing or writing the daemon Secret failed: a stable
/// label, safe to log and to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failed(pub &'static str);

/// A Paseo relay endpoint as the daemon and the apps take it: `host:port`,
/// with no scheme, path or credentials.
pub fn is_relay_endpoint(endpoint: &str) -> bool {
    endpoint.rsplit_once(':').is_some_and(|(host, port)| {
        !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            && port.parse::<u16>().is_ok_and(|port| port != 0)
    })
}

/// New pairing fields: a password of 32 random bytes, a server ID as Paseo's
/// own generator makes it (`srv_` and 9 random bytes, base64url), and a NaCl
/// box keypair.
pub fn generate() -> Result<Value, Failed> {
    let mut password = Zeroizing::new([0u8; 32]);
    let mut server_id = [0u8; 9];
    let mut secret = Zeroizing::new([0u8; 32]);
    for bytes in [&mut password[..], &mut server_id[..], &mut secret[..]] {
        getrandom::fill(bytes).map_err(|_| Failed("no_randomness"))?;
    }
    Ok(json!({
        "password": URL_SAFE_NO_PAD.encode(*password),
        "server_id": format!("srv_{}", URL_SAFE_NO_PAD.encode(server_id)),
        "daemon_keypair": keypair(&secret),
    }))
}

/// tweetnacl's `box.keyPair.fromSecretKey`, in the daemon's
/// `daemon-keypair.json` shape: the public key is X25519 of the clamped
/// secret and the base point, and the secret is kept unclamped.
fn keypair(secret: &[u8; 32]) -> Value {
    let public = MontgomeryPoint::mul_base_clamped(*secret);
    json!({
        "v": 2,
        "publicKeyB64": STANDARD.encode(public.as_bytes()),
        "secretKeyB64": STANDARD.encode(secret),
    })
}

/// The URL the daemon's `encodeOfferToFragmentUrl` builds for its
/// `ConnectionOfferV2`: the offer JSON, keys in the schema's order, as
/// unpadded base64url after `#offer=`. The relay is always TLS.
pub fn offer_url(server_id: &str, daemon_public_key: &str, relay_endpoint: &str) -> String {
    let offer = format!(
        r#"{{"v":2,"serverId":{},"daemonPublicKeyB64":{},"relay":{{"endpoint":{},"useTls":true}}}}"#,
        Value::from(server_id),
        Value::from(daemon_public_key),
        Value::from(relay_endpoint),
    );
    format!("{APP_BASE_URL}/#offer={}", URL_SAFE_NO_PAD.encode(offer))
}

#[path = "paseo_env.rs"]
mod environment;
use environment::{daemon_env, pairing, read_env};

/// The agent's display name from its desktop launch, the last tier winning
/// as in the launch precedence, or a name from its key.
fn label(payload: &Value, agent: &str) -> String {
    let named = [
        "/env_vars/BUZZ_ACP_DISPLAY_NAME",
        "/launch/policy_env/BUZZ_ACP_DISPLAY_NAME",
        "/launch/env/BUZZ_ACP_DISPLAY_NAME",
    ]
    .iter()
    .filter_map(|pointer| payload.pointer(pointer).and_then(Value::as_str))
    .map(str::trim)
    .rfind(|name| !name.is_empty() && !name.chars().any(char::is_control));
    match named {
        Some(name) => name.chars().take(MAX_LABEL).collect(),
        None => format!("Agent {}", &agent[..8]),
    }
}

/// `label` as an ID the daemon accepts, `^[a-z][a-z0-9-]*$`.
fn slug(label: &str) -> String {
    let mut slug = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(MAX_ID);
    let slug = slug.trim_end_matches('-');
    match slug.chars().next() {
        Some(c) if c.is_ascii_lowercase() => slug.to_owned(),
        Some(_) => format!("agent-{slug}"),
        None => "agent".to_owned(),
    }
}

/// A validated registration. The optional channel is frozen legacy metadata.
#[derive(Clone)]
struct AgentEntry {
    label: String,
    agent: String,
    legacy_channel: Option<String>,
    created_at: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,
}

/// Keep legacy IDs computed only among legacy registrations. New IDs depend
/// on their own key, except that a short-key collision expands both keys.
fn agent_list(mut entries: Vec<AgentEntry>) -> Value {
    entries.sort_by(|a, b| match (&a.legacy_channel, &b.legacy_channel) {
        (Some(_), Some(_)) => {
            (&a.label, &a.agent, &a.legacy_channel).cmp(&(&b.label, &b.agent, &b.legacy_channel))
        }
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => {
            (&a.created_at, &a.agent, &a.label).cmp(&(&b.created_at, &b.agent, &b.label))
        }
    });
    // Shared namespaces can contain registrations by several owners for one
    // agent. Choose one deterministically before assigning IDs or applying
    // the cap: prefer legacy metadata, then the ordering above.
    let mut agents = BTreeSet::new();
    entries.retain(|entry| agents.insert(entry.agent.clone()));
    let mut used: BTreeSet<String> = RESERVED_IDS.iter().map(|id| (*id).to_owned()).collect();
    let mut legacy_ids = BTreeMap::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.legacy_channel.is_some())
    {
        let mut id = slug(&entry.label);
        if used.contains(&id) {
            id = format!("{id}-{}", &entry.agent[..8]);
        }
        if used.contains(&id) {
            id = format!("{}-{}", slug(&entry.label), entry.agent);
        }
        used.insert(id.clone());
        legacy_ids.insert(entry.agent.clone(), id);
    }
    // Count before truncation: even a collision beyond the cap must expand
    // both keys, independently of timestamp and input ordering.
    let mut short_ids = BTreeMap::<String, usize>::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.legacy_channel.is_none())
    {
        let id = format!("{}-{}", slug(&entry.label), &entry.agent[..8]);
        *short_ids.entry(id).or_default() += 1;
    }
    Value::Array(
        entries
            .into_iter()
            .take(MAX_AGENTS)
            .map(|entry| {
                let id = match legacy_ids.get(&entry.agent) {
                    Some(id) => id.clone(),
                    None => {
                        let short = format!("{}-{}", slug(&entry.label), &entry.agent[..8]);
                        if used.contains(&short) || short_ids[&short] > 1 {
                            format!("{}-{}", slug(&entry.label), entry.agent)
                        } else {
                            short
                        }
                    }
                };
                let mut value = json!({"id": id, "label": entry.label, "agent": entry.agent});
                if let Some(channel) = entry.legacy_channel {
                    value["channel"] = channel.into();
                }
                value
            })
            .collect(),
    )
}

/// A label for a Kubernetes error. The error itself is never logged.
fn error_label(error: &kube::Error) -> &'static str {
    match error {
        kube::Error::Api(status) if status.code == 403 => "forbidden",
        kube::Error::Api(status) if status.code == 404 => "namespace_not_found",
        kube::Error::Api(_) => "rejected",
        _ => "unavailable",
    }
}

/// Writes each developer's daemon Secret, `paseo-daemon-<developer>`.
pub struct DaemonSecrets {
    client: kube::Client,
    namespace: String,
    relay_endpoint: String,
    /// The manager's Buzz relay, normalized: the daemon lists only agents
    /// registered on it.
    buzz_relay: String,
    /// Serializes listing the registrations with writing the Secret, so a
    /// slower writer never replaces a newer agent list.
    lock: tokio::sync::Mutex<()>,
}

impl DaemonSecrets {
    /// Secrets in [`NAMESPACE`], through the manager's Kubernetes client.
    /// Offers name `relay_endpoint`; the agent list names only agents
    /// registered on `buzz_relay`, the manager's own relay.
    pub fn new(
        client: kube::Client,
        relay_endpoint: &str,
        buzz_relay: &str,
    ) -> Result<Self, Failed> {
        if !is_relay_endpoint(relay_endpoint) {
            return Err(Failed("invalid_relay_endpoint"));
        }
        let buzz_relay =
            mobile_launch::relay_endpoint(buzz_relay).map_err(|_| Failed("invalid_relay_url"))?;
        Ok(Self {
            client,
            namespace: NAMESPACE.to_owned(),
            relay_endpoint: relay_endpoint.to_owned(),
            buzz_relay,
            lock: tokio::sync::Mutex::new(()),
        })
    }

    /// The owner's pairing details from the daemon Secret, never the profile.
    pub async fn pairing(&self, developer: &Developer) -> Result<Option<Value>, Failed> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        api.get_opt(&Self::name(&developer.id))
            .await
            .map_err(|e| Failed(error_label(&e)))?
            .map(|secret| pairing(&read_env(&secret)?, &self.relay_endpoint))
            .transpose()
    }

    /// The developer's daemon Secret, as the Terraform Deployment names it.
    pub fn name(developer: &str) -> String {
        format!("paseo-daemon-{developer}")
    }

    /// Valid registrations on the manager's relay, including agents that
    /// have never launched in a channel. Legacy entries come first; others
    /// are capped by registration creation time, then key.
    async fn agents(&self, developer: &Developer) -> Result<Value, Failed> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &developer.namespace);
        let list = api
            .list(
                &ListParams::default()
                    .labels(&format!("{LABEL}=v1"))
                    .limit(MAX_LISTED as u32),
            )
            .await
            .map_err(|_| Failed("registrations_unavailable"))?;
        let unread = list.items.len() > MAX_LISTED
            || list
                .metadata
                .continue_
                .as_ref()
                .is_some_and(|c| !c.is_empty());
        let entries: Vec<_> = list
            .items
            .iter()
            .take(MAX_LISTED)
            .filter_map(|secret| self.entry(secret, &developer.namespace))
            .collect();
        if unread || entries.len() > MAX_AGENTS {
            tracing::warn!(
                developer = %developer.id,
                listed = entries.len(),
                unread,
                limit = MAX_AGENTS,
                "Paseo agent list shortened: the developer has too many launch registrations"
            );
        }
        Ok(agent_list(entries))
    }

    /// One valid registration, with its optional frozen legacy channel.
    fn entry(&self, secret: &Secret, namespace: &str) -> Option<AgentEntry> {
        let legacy_channel = secret
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(CHANNEL_ANNOTATION))
            .filter(|channel| mobile_launch::is_channel_id(channel))
            .cloned();
        let raw = secret.data.as_ref()?.get(DATA_KEY)?;
        let saved: Registration = serde_json::from_slice(&raw.0).ok()?;
        let relay = saved.payload["relay_url"]
            .as_str()
            .and_then(|url| mobile_launch::relay_endpoint(url).ok())?;
        let valid = saved.version == 1
            && saved.relay == self.buzz_relay
            && relay == self.buzz_relay
            && secret.metadata.namespace.as_deref() == Some(namespace)
            && secret.metadata.name.as_deref()
                == Some(&Registration::name(saved.owner, saved.agent, &saved.relay));
        let agent = saved.agent.to_hex();
        valid.then(|| AgentEntry {
            label: label(&saved.payload, &agent),
            agent,
            legacy_channel,
            created_at: secret.metadata.creation_timestamp.clone(),
        })
    }

    /// Save `secret`, created or replacing `prior` at its resourceVersion.
    async fn save(
        &self,
        api: &Api<Secret>,
        prior: Option<&Secret>,
        mut secret: Secret,
        developer: &str,
    ) -> Result<bool, Failed> {
        let name = Self::name(developer);
        secret.type_ = Some("Opaque".into());
        secret.metadata.name = Some(name.clone());
        secret.metadata.namespace = Some(self.namespace.clone());
        secret.metadata.labels = Some(BTreeMap::from([
            (
                "app.kubernetes.io/managed-by".to_owned(),
                "buzz-sandbox-manager".to_owned(),
            ),
            ("voltage.cloud/developer".to_owned(), developer.to_owned()),
        ]));
        let saved = match prior.and_then(|prior| prior.metadata.resource_version.clone()) {
            Some(version) => {
                secret.metadata.resource_version = Some(version);
                api.replace(&name, &PostParams::default(), &secret).await
            }
            None => api.create(&PostParams::default(), &secret).await,
        };
        match saved {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(status)) if status.code == 409 => Ok(false),
            Err(error) => Err(Failed(error_label(&error))),
        }
    }

    /// Create the developer's Secret, or replace it at the resourceVersion
    /// just read, preserving pairing and gateway identity while updating model
    /// tokens and current registrations. A conflict with a concurrent writer is
    /// retried a bounded number of times.
    pub async fn write(&self, developer: &Developer, profile: &Value) -> Result<Value, Failed> {
        self.write_with_git_token(developer, profile, None).await
    }

    /// Write the dedicated Git token only into the daemon Secret.
    pub async fn write_with_git_token(
        &self,
        developer: &Developer,
        profile: &Value,
        token: Option<&GitReadToken>,
    ) -> Result<Value, Failed> {
        let _serialized = self.lock.lock().await;
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = Self::name(&developer.id);
        for _ in 0..WRITE_ATTEMPTS {
            let prior = api
                .get_opt(&name)
                .await
                .map_err(|e| Failed(error_label(&e)))?;
            let existing = prior.as_ref().map(read_env).transpose()?;
            let mut env = daemon_env(profile, &self.agents(developer).await?, existing)?;
            match token {
                Some(GitReadToken::Set(token)) if valid_git_read_token(token) => {
                    env.insert(GIT_READ_TOKEN_KEY.to_owned(), token.to_string());
                }
                Some(GitReadToken::Set(_)) => return Err(Failed("invalid_git_read_token")),
                Some(GitReadToken::Remove) => {
                    env.remove(GIT_READ_TOKEN_KEY);
                }
                None => {}
            }
            let pairing = pairing(&env, &self.relay_endpoint)?;
            let secret = Secret {
                string_data: Some(env),
                ..Default::default()
            };
            if self
                .save(&api, prior.as_ref(), secret, &developer.id)
                .await?
            {
                return Ok(pairing);
            }
        }
        Err(Failed("conflict"))
    }

    /// Read current configured daemon identities directly from Kubernetes,
    /// each bound to the owner whose configured developer Secret holds it.
    /// This classification survives delegation expiry and bounded relay lookups.
    /// It does not classify generic gateway keys or retired/deleted Secrets.
    pub async fn gateway_keys(
        &self,
        developers: &[(nostr::PublicKey, Developer)],
    ) -> Result<Vec<ManagedGateway>, Failed> {
        if developers.len() > MAX_DAEMON_IDENTITIES {
            return Err(Failed("too_many_daemons"));
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
            let mut keys = Vec::with_capacity(developers.len());
            for (owner, developer) in developers {
                let Some(secret) = api
                    .get_opt(&Self::name(&developer.id))
                    .await
                    .map_err(|error| Failed(error_label(&error)))?
                else {
                    continue;
                };
                let raw = secret
                    .data
                    .as_ref()
                    .and_then(|data| data.get("BUZZ_PRIVATE_KEY"))
                    .ok_or(Failed("invalid_gateway_key"))?;
                let key = std::str::from_utf8(&raw.0)
                    .ok()
                    .filter(|key| {
                        key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .ok_or(Failed("invalid_gateway_key"))?;
                let key = nostr::Keys::parse(key).map_err(|_| Failed("invalid_gateway_key"))?;
                keys.push(ManagedGateway {
                    owner: *owner,
                    key: key.public_key(),
                });
            }
            Ok(keys)
        })
        .await
        .map_err(|_| Failed("identity_timeout"))?
    }

    /// Replace only [`AGENTS_KEY`] in the developer's existing Secret, after
    /// a registration changed. Without a Secret there is nothing to update:
    /// the developer has not enrolled since Paseo daemons were enabled, and
    /// that enroll writes the whole Secret.
    pub async fn refresh_agents(&self, developer: &Developer) -> Result<(), Failed> {
        let _serialized = self.lock.lock().await;
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = Self::name(&developer.id);
        for _ in 0..WRITE_ATTEMPTS {
            let Some(prior) = api
                .get_opt(&name)
                .await
                .map_err(|e| Failed(error_label(&e)))?
            else {
                return Ok(());
            };
            // A list refresh must not write through an incomplete saved identity.
            pairing(&read_env(&prior)?, &self.relay_endpoint)?;
            let agents = self.agents(developer).await?;
            let current = prior
                .data
                .as_ref()
                .and_then(|data| data.get(AGENTS_KEY))
                .and_then(|value| std::str::from_utf8(&value.0).ok())
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
            if current.as_ref() == Some(&agents) {
                return Ok(());
            }
            let mut data = prior.data.clone().unwrap_or_default();
            data.remove(AGENTS_KEY);
            let secret = Secret {
                data: Some(data),
                string_data: Some(BTreeMap::from([(
                    AGENTS_KEY.to_owned(),
                    agents.to_string(),
                )])),
                ..Default::default()
            };
            if self.save(&api, Some(&prior), secret, &developer.id).await? {
                return Ok(());
            }
        }
        Err(Failed("conflict"))
    }
}

#[cfg(test)]
#[path = "paseo_tests.rs"]
mod tests;
