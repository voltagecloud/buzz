//! Owner-authorized mobile starts. Launch credentials stay in a namespaced
//! Secret on the manager's substrate, never on the phone or in relay history.

use std::collections::BTreeMap;

use buzz_backend_kubernetes::{provider, wire};
use k8s_openapi::api::core::v1::Secret;
use kube::{api::PostParams, Api};
use nostr::{Event, Keys, PublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::manager::refusal;

const DATA_KEY: &str = "launch.json";
const LABEL: &str = "buzz.voltage.cloud/mobile-launch";

fn store_failed() -> Value {
    refusal(
        wire::ErrorCode::LaunchStoreFailed,
        "the saved mobile launch could not be read or updated; retry",
    )
}

fn relay_endpoint(text: &str) -> Result<String, Value> {
    let url = url::Url::parse(text).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "wss" | "ws") || url.host_str().is_none() {
        return Err(invalid());
    }
    // Desktop supports relay paths and queries. Preserve the full endpoint
    // in the community binding; distinct paths on one host must not collide.
    if url.path() == "/" && url.query().is_none() && url.fragment().is_none() {
        Ok(url.as_str().trim_end_matches('/').to_owned())
    } else {
        Ok(url.to_string())
    }
}

fn invalid() -> Value {
    refusal(
        wire::ErrorCode::InvalidStart,
        "start requires the owner's signed channel message addressing this agent",
    )
}

/// A launch snapshot registered by a successful, agent-signed deploy.
/// Deliberately has no Debug implementation: the payload contains credentials.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Registration {
    version: u32,
    owner: PublicKey,
    agent: PublicKey,
    relay: String,
    payload: Value,
}

impl Registration {
    /// Bind a desktop launch snapshot to its verified owner, agent and relay.
    pub fn new(owner: PublicKey, agent: PublicKey, payload: Value) -> Result<Self, Value> {
        let relay = payload["relay_url"]
            .as_str()
            .and_then(|text| relay_endpoint(text).ok())
            .ok_or_else(|| {
                refusal(
                    wire::ErrorCode::InvalidLaunchRegistration,
                    "desktop launch registration requires a valid ws:// or wss:// relay URL",
                )
            })?;
        Ok(Self {
            version: 1,
            owner,
            agent,
            relay,
            payload,
        })
    }

    fn name(owner: PublicKey, agent: PublicKey, relay: &str) -> String {
        let hash = Sha256::digest(format!("{owner}\n{agent}\n{relay}").as_bytes());
        let hex: String = hash[..20].iter().map(|b| format!("{b:02x}")).collect();
        format!("buzz-mobile-launch-{hex}")
    }

    /// One atomic create or resourceVersion-checked replace. A conflict is
    /// bounded; a failed save is returned instead of claiming full success.
    pub async fn save(&self, client: kube::Client, namespace: &str) -> Result<(), Value> {
        let api: Api<Secret> = Api::namespaced(client, namespace);
        let name = Self::name(self.owner, self.agent, &self.relay);
        let text = serde_json::to_string(self).map_err(|_| store_failed())?;
        for _ in 0..3 {
            let prior = api.get_opt(&name).await.map_err(|_| store_failed())?;
            if let Some(prior) = &prior {
                if prior
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(LABEL))
                    .map(String::as_str)
                    != Some("v1")
                {
                    return Err(store_failed());
                }
            }
            let mut secret = Secret {
                string_data: Some(BTreeMap::from([(DATA_KEY.into(), text.clone())])),
                type_: Some("Opaque".into()),
                ..Default::default()
            };
            secret.metadata.name = Some(name.clone());
            secret.metadata.namespace = Some(namespace.into());
            secret.metadata.labels = Some(BTreeMap::from([(LABEL.into(), "v1".into())]));
            secret.metadata.resource_version = prior
                .as_ref()
                .and_then(|s| s.metadata.resource_version.clone());
            let saved = if prior.is_some() {
                api.replace(&name, &PostParams::default(), &secret).await
            } else {
                api.create(&PostParams::default(), &secret).await
            };
            match saved {
                Ok(_) => return Ok(()),
                Err(kube::Error::Api(error)) if error.code == 409 => {}
                Err(_) => return Err(store_failed()),
            }
        }
        Err(store_failed())
    }
}

/// A verified owner mention, with its immutable channel and thread binding.
pub(crate) struct Start {
    owner: PublicKey,
    agent: PublicKey,
    relay: String,
    scope: Value,
    replay_floor: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartPayload {
    agent_pubkey: PublicKey,
    relay_url: String,
    trigger: Event,
}

impl Start {
    /// Verify the owner's signed mention and derive its immutable thread scope.
    pub fn admit(payload: Value, owner: PublicKey, request_time: u64) -> Result<Self, Value> {
        let payload: StartPayload = serde_json::from_value(payload).map_err(|_| invalid())?;
        let event = payload.trigger;
        let tags: Vec<&[String]> = event.tags.iter().map(|tag| tag.as_slice()).collect();
        let channels: Vec<_> = tags
            .iter()
            .filter(|t| t.first().map(String::as_str) == Some("h"))
            .collect();
        let addressed = tags.iter().any(|t| {
            t.first().map(String::as_str) == Some("p")
                && t.get(1).map(String::as_str) == Some(payload.agent_pubkey.to_hex().as_str())
        });
        if event.verify().is_err()
            || event.pubkey != owner
            || payload.agent_pubkey == owner
            || !matches!(event.kind.as_u16(), 9 | 40002)
            || event.created_at.as_secs() > request_time.saturating_add(60)
            || !addressed
            || channels.len() != 1
            || channels[0].len() != 2
        {
            return Err(invalid());
        }
        let marked = |marker: &str| -> Vec<&str> {
            tags.iter()
                .filter(|t| {
                    t.first().map(String::as_str) == Some("e")
                        && t.get(3).map(String::as_str) == Some(marker)
                })
                .filter_map(|t| t.get(1).map(String::as_str))
                .collect()
        };
        let roots = marked("root");
        let replies = marked("reply");
        let unmarked = tags.iter().any(|tag| {
            tag.first().map(String::as_str) == Some("e")
                && !matches!(
                    tag.get(3).map(String::as_str),
                    Some("root" | "reply" | "mention")
                )
        });
        if roots.len() > 1 || replies.len() > 1 || unmarked {
            return Err(invalid());
        }
        let id = event.id.to_hex();
        let root = roots.first().or(replies.first()).copied().unwrap_or(&id);
        let scope = json!({"channel_id": channels[0][1], "thread_root": root});
        provider::check_sandbox_scope(&json!({"sandbox": scope})).map_err(|_| invalid())?;
        Ok(Self {
            owner,
            agent: payload.agent_pubkey,
            relay: relay_endpoint(&payload.relay_url)?,
            scope,
            replay_floor: event.created_at.as_secs(),
        })
    }

    /// The addressed agent, used to confine the managed provider target.
    pub fn agent(&self) -> PublicKey {
        self.agent
    }

    /// Read only the verified owner's namespace. Recheck all saved identity
    /// bindings and attestation before invoking the ordinary confined provider.
    pub async fn request(
        self,
        client: kube::Client,
        namespace: &str,
    ) -> Result<wire::Request, Value> {
        let api: Api<Secret> = Api::namespaced(client, namespace);
        let name = Registration::name(self.owner, self.agent, &self.relay);
        let secret = api.get_opt(&name).await.map_err(|_| store_failed())?.ok_or_else(|| {
            refusal(wire::ErrorCode::LaunchNotRegistered,
                "start this agent once from desktop after the manager upgrade to register mobile launches")
        })?;
        let data = secret
            .data
            .as_ref()
            .and_then(|d| d.get(DATA_KEY))
            .ok_or_else(store_failed)?;
        if secret
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(LABEL))
            .map(String::as_str)
            != Some("v1")
        {
            return Err(store_failed());
        }
        let saved: Registration = serde_json::from_slice(&data.0).map_err(|_| store_failed())?;
        let key = saved.payload["private_key_nsec"]
            .as_str()
            .ok_or_else(store_failed)?;
        if saved.version != 1
            || saved.owner != self.owner
            || saved.agent != self.agent
            || saved.relay != self.relay
            || Keys::parse(key).map(|keys| keys.public_key()).ok() != Some(self.agent)
            || relay_endpoint(
                saved.payload["relay_url"]
                    .as_str()
                    .ok_or_else(store_failed)?,
            )? != self.relay
        {
            return Err(store_failed());
        }
        let mut payload = json!({"op": "deploy", "agent": saved.payload,
            "provider_config": {"sandbox": self.scope,
                "identity_policy": {"agent_pubkey": self.agent, "owner_pubkey": self.owner}}});
        // Replay is invocation state. Never retain an earlier thread's floor.
        for tier in ["env", "policy_env"] {
            if let Some(env) = payload
                .get_mut("agent")
                .and_then(|a| a.get_mut("launch"))
                .and_then(|launch| launch.get_mut(tier))
                .and_then(Value::as_object_mut)
            {
                env.retain(|key, _| !key.eq_ignore_ascii_case("BUZZ_ACP_REPLAY_FLOOR"));
            }
        }
        payload["agent"]["launch"]["env"]["BUZZ_ACP_REPLAY_FLOOR"] =
            self.replay_floor.to_string().into();
        let request = provider::parse(payload).map_err(|_| store_failed())?;
        let wire::Request::Deploy(deploy) = &request else {
            return Err(store_failed());
        };
        provider::check_sandbox_request(deploy).map_err(|_| store_failed())?;
        Ok(request)
    }
}

#[cfg(test)]
mod tests;
