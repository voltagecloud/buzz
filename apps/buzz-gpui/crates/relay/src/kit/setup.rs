//! Channel creation with setup (`features/channel-templates/setup.ts`):
//! the frozen intent of one creation, saved on this device before anything
//! is sent (`buzz-channel-setup.v2` receipts), and the 9007 operations the
//! outbox still holds after a restart (`restoredChannelCreation`).
//!
//! Receipts record intent and progress only. Signing, publication and
//! delivery evidence stay in the outbox.

use super::{identifier, parse_lineup};
use crate::discovery::is_uuid;
use crate::event::kinds;
use crate::lifecycle::MAX_TTL;
use crate::lifecycle::{NewChannel, canonical_name};
use crate::outbox::Operation;
use crate::store::{Partition, Store};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

/// The view-state key of this device's receipts in a community.
pub const RECEIPTS_KEY: &str = "buzz-channel-setup.v2";
/// One receipt's JSON budget.
const RECEIPT_BYTES: usize = 64 * 1024;
const MAX_RECEIPTS: usize = 256;
const RECEIPTS_BYTES: usize = 2 * 1024 * 1024;

/// What a template adds to a new channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSetup {
    pub canvas: String,
    pub agents: Vec<String>,
    /// A sidebar section. Receipts from before personal groups were
    /// retired may still carry a `groupSource`; it is ignored.
    pub group_id: String,
    pub template_id: String,
}

impl ChannelSetup {
    pub fn is_empty(&self) -> bool {
        self.canvas.is_empty()
            && self.agents.is_empty()
            && self.group_id.is_empty()
            && self.template_id.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CreationVisibility {
    Open,
    Private,
}

/// `ChannelCreationInput`: the frozen request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelCreationInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub visibility: CreationVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    /// NIP-CL labels to create the channel with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<ChannelSetup>,
}

impl ChannelCreationInput {
    pub fn from_new(channel: &NewChannel, setup: Option<ChannelSetup>) -> Self {
        let description = channel.description.trim();
        Self {
            name: canonical_name(&channel.name),
            description: (!description.is_empty()).then(|| description.to_owned()),
            visibility: if channel.private {
                CreationVisibility::Private
            } else {
                CreationVisibility::Open
            },
            ttl_seconds: channel.ttl,
            labels: channel
                .labels
                .iter()
                .enumerate()
                .filter(|(at, label)| !channel.labels[..*at].contains(label))
                .map(|(_, label)| label.clone())
                .collect(),
            setup: setup.filter(|setup| !setup.is_empty()),
        }
    }

    pub fn channel(&self) -> NewChannel {
        NewChannel {
            name: self.name.clone(),
            description: self.description.clone().unwrap_or_default(),
            private: self.visibility == CreationVisibility::Private,
            ttl: self.ttl_seconds,
            labels: self.labels.clone(),
        }
    }

    /// The static part of `preflight`: details and selections are well
    /// formed. Availability is checked against the session.
    pub fn check(&self) -> Result<(), String> {
        let name = canonical_name(&self.name);
        let bad_details = name.is_empty()
            || name.chars().count() > 120
            || self.description.as_ref().is_some_and(|description| {
                description.chars().count() > 1000 || description.contains("Buzz session (")
            })
            || self
                .ttl_seconds
                .is_some_and(|ttl| ttl == 0 || ttl > MAX_TTL);
        if bad_details {
            return Err(
                "Check the channel name, description, privacy and duration before creating it"
                    .into(),
            );
        }
        crate::channel_labels::label_tags(&self.labels)?;
        let Some(setup) = &self.setup else {
            return Ok(());
        };
        parse_lineup(&json!({"teamIds": [], "agents": setup.agents, "canvas": setup.canvas}))?;
        let group_ok = setup.group_id.is_empty()
            || (setup.group_id.len() <= 256 && !setup.group_id.trim().is_empty());
        if !group_ok || (!setup.template_id.is_empty() && !identifier(&setup.template_id)) {
            return Err("Invalid template or group selection".into());
        }
        Ok(())
    }
}

/// One creation's receipt (`Progress`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub version: u8,
    pub id: String,
    pub input: ChannelCreationInput,
    pub created: bool,
    pub group_done: bool,
    pub canvas_done: bool,
    pub added: Vec<String>,
    pub accepted: Vec<String>,
    /// Step (`create`, `canvas`, `member:<key>`) to event id.
    pub operations: BTreeMap<String, String>,
}

impl Progress {
    pub fn new(input: ChannelCreationInput) -> Self {
        Self {
            version: 2,
            id: uuid::Uuid::new_v4().to_string(),
            input,
            created: false,
            group_done: false,
            canvas_done: false,
            added: Vec::new(),
            accepted: Vec::new(),
            operations: BTreeMap::new(),
        }
    }
}

/// This device's receipts in one community, by channel id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipts(pub BTreeMap<String, Progress>);

impl Receipts {
    /// Unreadable receipts read as none: they never authorize a write.
    pub fn load(store: &Store, partition: &Partition) -> Result<Self, String> {
        let saved = store
            .view_state(partition, RECEIPTS_KEY)
            .map_err(|error| error.to_string())?;
        Ok(saved
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default())
    }

    /// Saves `progress` within the budgets. On failure nothing is replaced
    /// and the caller must not continue its remote writes.
    pub fn write(store: &Store, partition: &Partition, progress: &Progress) -> Result<(), String> {
        let mut receipts = Self::load(store, partition)?;
        receipts.0.insert(progress.id.clone(), progress.clone());
        receipts.check(&progress.id)?;
        let json = serde_json::to_string(&receipts).map_err(|error| error.to_string())?;
        store
            .set_view_state(partition, RECEIPTS_KEY, Some(&json))
            .map_err(|error| error.to_string())
    }

    pub fn remove(store: &Store, partition: &Partition, id: &str) -> Result<(), String> {
        let mut receipts = Self::load(store, partition)?;
        if receipts.0.remove(id).is_none() {
            return Ok(());
        }
        let json = serde_json::to_string(&receipts).map_err(|error| error.to_string())?;
        store
            .set_view_state(
                partition,
                RECEIPTS_KEY,
                (!receipts.0.is_empty()).then_some(json.as_str()),
            )
            .map_err(|error| error.to_string())
    }

    /// Whether this channel's creation was admitted here.
    pub fn admitted(&self, id: &str) -> bool {
        self.0
            .get(id)
            .is_some_and(|p| p.version == 2 && p.id == id && p.created)
    }

    fn check(&self, id: &str) -> Result<(), String> {
        let size = |p: &Progress| serde_json::to_vec(p).map_or(usize::MAX, |b| b.len());
        if self.0.get(id).is_some_and(|p| size(p) > RECEIPT_BYTES) {
            return Err("Channel setup exceeds its local receipt budget".into());
        }
        let total: usize = self.0.values().map(size).sum();
        if self.0.len() > MAX_RECEIPTS || total > RECEIPTS_BYTES {
            return Err(
                "Too many unfinished channel setups on this device. No new setup write was sent."
                    .into(),
            );
        }
        Ok(())
    }
}

/// A creation the outbox still holds (`PendingChannelCreation`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingCreation {
    pub id: String,
    pub operation: nostr::prelude::EventId,
    pub input: ChannelCreationInput,
}

/// `parseChannelCreation`: a 9007 operation in exactly the shape this app
/// sends, as the frozen input it was sent for.
pub fn pending_creation(op: &Operation) -> Option<PendingCreation> {
    if op.kind() != kinds::CREATE_CHANNEL || !op.event.content.is_empty() {
        return None;
    }
    let tags: Vec<&[String]> = op
        .event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().map(String::as_str) != Some("client-id"))
        .collect();
    let pair = |index: usize, name: &str| -> Option<&str> {
        match tags.get(index)? {
            [key, value] if key == name => Some(value.as_str()),
            _ => None,
        }
    };
    let id = pair(0, "h")?;
    let name = pair(1, "name")?;
    let visibility = match pair(2, "visibility")? {
        "open" => CreationVisibility::Open,
        "private" => CreationVisibility::Private,
        _ => return None,
    };
    if pair(3, "channel_type")? != "stream" || !is_uuid(id) || canonical_name(name).is_empty() {
        return None;
    }
    // Label `t` tags close the command.
    let first_label = tags
        .iter()
        .position(|tag| tag.first().map(String::as_str) == Some("t"))
        .unwrap_or(tags.len())
        .max(4);
    let (optional, label_tags) = tags[4..].split_at(first_label - 4);
    let labels: Vec<String> = label_tags
        .iter()
        .map(|tag| tag.get(1).cloned().unwrap_or_default())
        .collect();
    if label_tags
        .iter()
        .any(|tag| tag.len() != 2 || tag[0] != "t" || !crate::channel_labels::valid_label(&tag[1]))
        || labels.len() > crate::channel_labels::MAX_CHANNEL_LABELS
        || labels
            .iter()
            .enumerate()
            .any(|(at, label)| labels[..at].contains(label))
    {
        return None;
    }
    let names: Vec<&str> = optional
        .iter()
        .map(|tag| tag.first().map_or("", String::as_str))
        .collect();
    if ![&[][..], &["about"], &["ttl"], &["about", "ttl"]].contains(&names.as_slice())
        || optional.iter().any(|tag| tag.len() != 2)
    {
        return None;
    }
    let value = |key: &str| {
        optional
            .iter()
            .find(|tag| tag[0] == key)
            .map(|tag| tag[1].as_str())
    };
    let description = value("about").map(str::trim).filter(|d| !d.is_empty());
    if description.is_some_and(|d| d.contains("Buzz session (")) {
        return None;
    }
    let ttl_seconds = match value("ttl") {
        None => None,
        Some(ttl) => match ttl.parse::<u64>() {
            Ok(ttl) if ttl > 0 && ttl <= MAX_TTL => Some(ttl),
            _ => return None,
        },
    };
    Some(PendingCreation {
        id: id.to_owned(),
        operation: op.id(),
        input: ChannelCreationInput {
            name: canonical_name(name),
            description: description.map(str::to_owned),
            visibility,
            ttl_seconds,
            labels,
            setup: None,
        },
    })
}

/// A notice that a channel was created but its setup did not finish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupNotice {
    pub id: String,
    pub name: String,
    pub error: String,
}

pub const SETUP_NOT_CONTINUED: &str = "Channel confirmed. Saved setup was not continued; check its group, Canvas and members before finishing them manually.";
pub const TEMPLATE_NOT_CONTINUED: &str = "Channel confirmed. Template setup was not continued; check its group, Canvas and members before finishing them manually.";
