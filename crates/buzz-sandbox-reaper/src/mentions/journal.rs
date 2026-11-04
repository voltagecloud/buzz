//! One CAS-protected inbox and lease per registered identity and community.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use kube::{api::PostParams, Api};
use nostr::{Event, Timestamp};
use serde::{Deserialize, Serialize};

use super::{registry::Identity, Error};

const LABEL: &str = "buzz.voltage.cloud/mention-inbox";
const DATA: &str = "inbox.json";
const MAX_BYTES: usize = 512 * 1024;
const MAX_RECEIPTS: usize = 4096;
pub(super) const LEASE_SECONDS: u64 = 360;
pub(super) const OVERLAP_SECONDS: u64 = 120;

#[derive(Deserialize, Serialize)]
pub(super) struct Pending {
    pub event: Event,
    pub attempts: u8,
    pub retry_at: u64,
    pub notice: Option<Event>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Notice {
    pub event: Event,
    pub attempts: u8,
}

#[derive(Deserialize, Serialize)]
pub(super) struct State {
    version: u32,
    holder: String,
    lease_until: u64,
    /// Events older than first activation never trigger historical launches.
    pub activated: u64,
    /// Advance only after a complete historical subscription, never per event.
    pub since: u64,
    pub completed: BTreeMap<String, u64>,
    pub pending: Option<Pending>,
    /// Undeliverable replies must not block other channels' launches.
    #[serde(default)]
    pub outbox: Vec<Notice>,
}

pub(super) struct Journal {
    api: Api<Secret>,
    secret: Secret,
    holder: String,
    pub state: State,
}

impl Journal {
    pub async fn acquire(
        client: kube::Client,
        identity: &Identity,
        holder: &str,
        activated: u64,
    ) -> Result<Self, Error> {
        let api: Api<Secret> = Api::namespaced(client, &identity.namespace);
        let name = identity
            .name
            .replacen("buzz-mobile-launch-", "buzz-mention-inbox-", 1);
        let prior = api.get_opt(&name).await.map_err(|_| Error::Storage)?;
        let now = Timestamp::now().as_secs();
        let (secret, state) = if let Some(secret) = prior {
            if secret
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL))
                .map(String::as_str)
                != Some("v1")
            {
                return Err(Error::Storage);
            }
            let bytes = &secret
                .data
                .as_ref()
                .and_then(|d| d.get(DATA))
                .ok_or(Error::Storage)?
                .0;
            if bytes.len() > MAX_BYTES {
                return Err(Error::Capacity);
            }
            let state: State = serde_json::from_slice(bytes).map_err(|_| Error::Storage)?;
            if state.version != 1 || state.completed.len() > MAX_RECEIPTS || state.outbox.len() > 32
            {
                return Err(Error::Storage);
            }
            if state.holder != holder && state.lease_until > now {
                return Err(Error::Lease);
            }
            (secret, state)
        } else {
            let mut secret = Secret {
                type_: Some("Opaque".into()),
                ..Default::default()
            };
            secret.metadata.name = Some(name);
            secret.metadata.namespace = Some(identity.namespace.clone());
            secret.metadata.labels = Some(BTreeMap::from([(LABEL.into(), "v1".into())]));
            (
                secret,
                State {
                    version: 1,
                    holder: holder.into(),
                    lease_until: 0,
                    activated,
                    since: activated,
                    completed: BTreeMap::new(),
                    pending: None,
                    outbox: Vec::new(),
                },
            )
        };
        let mut journal = Self {
            api,
            secret,
            holder: holder.into(),
            state,
        };
        journal.save().await?;
        Ok(journal)
    }

    /// CAS fences a previous holder even if it completes after its lease expires.
    pub async fn save(&mut self) -> Result<(), Error> {
        self.state.holder.clone_from(&self.holder);
        self.state.lease_until = Timestamp::now().as_secs() + LEASE_SECONDS;
        let text = serde_json::to_string(&self.state).map_err(|_| Error::Storage)?;
        if text.len() > MAX_BYTES
            || self.state.completed.len() > MAX_RECEIPTS
            || self.state.outbox.len() > 32
        {
            return Err(Error::Capacity);
        }
        self.secret.data = None;
        self.secret.string_data = Some(BTreeMap::from([(DATA.into(), text)]));
        let name = self.secret.metadata.name.as_deref().ok_or(Error::Storage)?;
        self.secret = if self.secret.metadata.resource_version.is_some() {
            self.api
                .replace(name, &PostParams::default(), &self.secret)
                .await
        } else {
            self.api.create(&PostParams::default(), &self.secret).await
        }
        .map_err(|e| match e {
            kube::Error::Api(e) if e.code == 409 => Error::Lease,
            _ => Error::Storage,
        })?;
        Ok(())
    }

    pub async fn enqueue(&mut self, event: Event) -> Result<bool, Error> {
        if self.state.completed.contains_key(&event.id.to_hex()) {
            return Ok(false);
        }
        if self.state.pending.is_some() || self.state.completed.len() >= MAX_RECEIPTS {
            return Err(Error::Capacity);
        }
        self.state.pending = Some(Pending {
            event,
            attempts: 0,
            retry_at: 0,
            notice: None,
        });
        self.save().await?;
        Ok(true)
    }

    pub async fn complete(&mut self) -> Result<(), Error> {
        let pending = self.state.pending.take().ok_or(Error::Storage)?;
        if let Some(event) = pending.notice {
            self.state.outbox.push(Notice { event, attempts: 0 });
        }
        self.state.completed.insert(
            pending.event.id.to_hex(),
            pending.event.created_at.as_secs(),
        );
        self.save().await
    }

    pub async fn checkpoint(&mut self, connected_at: u64) -> Result<(), Error> {
        self.state.since = self
            .state
            .since
            .max(connected_at.saturating_sub(OVERLAP_SECONDS));
        self.state
            .completed
            .retain(|_, timestamp| *timestamp >= self.state.since);
        self.save().await
    }
}
