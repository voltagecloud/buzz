//! One bounded, CAS-protected admission journal per origin owner and community.
//! A missing/ambiguous write acknowledgement propagates; replay converges on the
//! same receipt rather than replenishing the task's budget.

use buzz_sdk::sharing::{budget::Budget, policy::Task};
use k8s_openapi::api::core::v1::Secret;
use kube::{api::PostParams, Api};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const LABEL: &str = "buzz.voltage.cloud/shared-admissions";
const DATA: &str = "admissions.json";
const MAX_TASKS: usize = 512;
const MAX_BYTES: usize = 900 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Refused(#[from] buzz_sdk::sharing::budget::Error),
    #[error("task admission storage unavailable; retry this task")]
    Storage,
    #[error("task admission capacity reached; wait for older tasks to expire")]
    Capacity,
    #[error("task admission journal is invalid; operator repair is required")]
    Journal,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    relay: String,
    tasks: BTreeMap<String, Budget>,
}

/// Persist an authorized task before either startup or harness delivery. The
/// namespace comes from the initial agent's verified, configured owner.
pub(crate) async fn admit(
    client: kube::Client,
    namespace: &str,
    relay: &str,
    task: &Task,
    now: u64,
) -> Result<(), Error> {
    let relay = crate::mobile_launch::relay_endpoint(relay).map_err(|_| Error::Journal)?;
    let digest = Sha256::digest(relay.as_bytes());
    let hash: String = digest[..20].iter().map(|b| format!("{b:02x}")).collect();
    let name = format!("buzz-shared-admissions-{hash}");
    let api: Api<Secret> = Api::namespaced(client, namespace);
    for _ in 0..3 {
        let prior = api.get_opt(&name).await.map_err(|_| Error::Storage)?;
        let mut journal = if let Some(secret) = &prior {
            if secret
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL))
                .map(String::as_str)
                != Some("v1")
            {
                return Err(Error::Journal);
            }
            let bytes = &secret
                .data
                .as_ref()
                .and_then(|d| d.get(DATA))
                .ok_or(Error::Journal)?
                .0;
            if bytes.len() > MAX_BYTES {
                return Err(Error::Capacity);
            }
            let journal: Journal = serde_json::from_slice(bytes).map_err(|_| Error::Journal)?;
            if journal.version != 1 || journal.relay != relay || journal.tasks.len() > MAX_TASKS {
                return Err(Error::Journal);
            }
            journal
        } else {
            Journal {
                version: 1,
                relay: relay.clone(),
                tasks: BTreeMap::new(),
            }
        };
        journal.tasks.retain(|_, budget| !budget.expired(now));
        let key = Budget::key(task);
        if !journal.tasks.contains_key(&key) {
            if journal.tasks.len() >= MAX_TASKS {
                return Err(Error::Capacity);
            }
            journal.tasks.insert(key.clone(), Budget::new(task, now)?);
        }
        let budget = journal.tasks.get_mut(&key).ok_or(Error::Journal)?;
        budget.admit(task, now)?;
        let body = serde_json::to_string(&journal).map_err(|_| Error::Journal)?;
        if body.len() > MAX_BYTES {
            return Err(Error::Capacity);
        }
        let mut secret = Secret {
            type_: Some("Opaque".into()),
            string_data: Some(BTreeMap::from([(DATA.into(), body)])),
            ..Default::default()
        };
        secret.metadata.name = Some(name.clone());
        secret.metadata.namespace = Some(namespace.into());
        secret.metadata.labels = Some(BTreeMap::from([(LABEL.into(), "v1".into())]));
        secret.metadata.resource_version = prior.and_then(|s| s.metadata.resource_version);
        let saved = if secret.metadata.resource_version.is_some() {
            api.replace(&name, &PostParams::default(), &secret).await
        } else {
            api.create(&PostParams::default(), &secret).await
        };
        match saved {
            Ok(_) => return Ok(()),
            Err(kube::Error::Api(e)) if e.code == 409 => continue,
            Err(_) => return Err(Error::Storage),
        }
    }
    Err(Error::Storage)
}

#[cfg(test)]
mod tests;
