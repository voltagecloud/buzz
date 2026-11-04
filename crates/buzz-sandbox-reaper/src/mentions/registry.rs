//! Only desktop-registered identities on the configured relay may be watched.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use kube::{api::ListParams, Api};
use nostr::{Keys, PublicKey, Tag, Timestamp};

use super::{Error, MAX_AGENTS};
use crate::manager::Manager;
use crate::mobile_launch::{relay_endpoint, Registration, DATA_KEY, LABEL};

/// No Debug: this holds the agent's credentials, never the owner's key.
pub(super) struct Identity {
    pub name: String,
    pub namespace: String,
    pub revision: String,
    pub registered_at: u64,
    pub owner: PublicKey,
    pub keys: Keys,
    pub auth: Tag,
    pub relay: String,
}

impl Identity {
    pub fn read(
        secret: Secret,
        namespace: &str,
        owner: PublicKey,
        relay: &str,
    ) -> Result<Self, Error> {
        let text = secret
            .data
            .as_ref()
            .and_then(|data| data.get(DATA_KEY))
            .ok_or(Error::Registration)?;
        let saved: Registration =
            serde_json::from_slice(&text.0).map_err(|_| Error::Registration)?;
        let key = saved.payload["private_key_nsec"]
            .as_str()
            .ok_or(Error::Registration)?;
        let keys = Keys::parse(key).map_err(|_| Error::Registration)?;
        let auth = saved.payload["auth_tag"]
            .as_str()
            .ok_or(Error::Registration)?;
        let expected_relay = relay_endpoint(relay).map_err(|_| Error::Registration)?;
        if saved.version != 1
            || saved.owner != owner
            || saved.agent != keys.public_key()
            || saved.relay != expected_relay
            || relay_endpoint(
                saved.payload["relay_url"]
                    .as_str()
                    .ok_or(Error::Registration)?,
            )
            .map_err(|_| Error::Registration)?
                != expected_relay
            || buzz_sdk::nip_oa::verify_auth_tag_for_auth_event(
                auth,
                &saved.agent,
                Timestamp::now().as_secs(),
            )
            .map_err(|_| Error::Registration)?
                != owner
            || secret.metadata.namespace.as_deref() != Some(namespace)
            || secret
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get(LABEL))
                .map(String::as_str)
                != Some("v1")
        {
            return Err(Error::Registration);
        }
        let name = Registration::name(owner, saved.agent, &expected_relay);
        if secret.metadata.name.as_deref() != Some(&name) {
            return Err(Error::Registration);
        }
        Ok(Self {
            name,
            namespace: namespace.into(),
            owner,
            keys,
            revision: secret
                .metadata
                .resource_version
                .ok_or(Error::Registration)?,
            registered_at: secret
                .metadata
                .creation_timestamp
                .as_ref()
                .and_then(|time| u64::try_from(time.0.timestamp()).ok())
                .ok_or(Error::Registration)?,
            auth: buzz_sdk::nip_oa::parse_auth_tag(auth).map_err(|_| Error::Registration)?,
            relay: expected_relay,
        })
    }
}

pub(super) async fn discover(
    manager: &Manager,
    relay: &str,
) -> Result<BTreeMap<String, Identity>, Error> {
    let mut found = BTreeMap::new();
    for (owner, developer) in manager.mention_bindings() {
        let api: Api<Secret> = Api::namespaced(manager.cluster(), &developer.namespace);
        let list = api
            .list(
                &ListParams::default()
                    .labels(&format!("{LABEL}=v1"))
                    .limit((MAX_AGENTS + 1) as u32),
            )
            .await
            .map_err(|_| Error::Storage)?;
        if list
            .metadata
            .continue_
            .as_ref()
            .is_some_and(|v| !v.is_empty())
            || found.len() + list.items.len() > MAX_AGENTS
        {
            return Err(Error::Capacity);
        }
        for secret in list.items {
            // Other community registrations are normal; never connect to their URLs.
            let raw = secret
                .data
                .as_ref()
                .and_then(|d| d.get(DATA_KEY))
                .ok_or(Error::Registration)?;
            let saved: Registration =
                serde_json::from_slice(&raw.0).map_err(|_| Error::Registration)?;
            if saved.relay != relay_endpoint(relay).map_err(|_| Error::Registration)? {
                continue;
            }
            let identity = Identity::read(secret, &developer.namespace, owner, relay)?;
            found.insert(
                format!("{}/{}", identity.namespace, identity.name),
                identity,
            );
        }
    }
    Ok(found)
}
