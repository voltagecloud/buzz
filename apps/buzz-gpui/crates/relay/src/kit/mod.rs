//! Channel templates and teams (`features/channel-templates/model.ts`,
//! `capability.ts` decode, `src-tauri` kit cipher): the viewer's private,
//! community-scoped recipes in kind 30078 records, NIP-44 encrypted to
//! themselves.
//!
//! A record's coordinate and admission rules are buzz-app's native ones,
//! used from `buzz_desktop::relay::kit`; the model checks here add the
//! TypeScript layer's messages and its 16 KiB record budget. Reading and
//! writing the records belongs to the session (`session/kit.rs`).

pub mod me;
pub mod payload;
pub mod setup;

use crate::discovery::is_uuid;
use crate::event::{Verified, kinds};
use crate::members::is_key;
use crate::socket::{Filter, filter};
use buzz_desktop::relay::kit as native;
use buzz_identity::Identity;
use nostr::prelude::EventId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub use crate::session::{CreationSnapshot, KitSnapshot, KitStatus};
pub use native::{KIT_TAG, MANIFEST_TAG, ME_KIT_TAG, PAYLOAD_TAG, TEXT_TAG};
pub use payload::{TeamManifest, TeamPayload};

/// Largest serialized recipe (`KIT_RECORD_BYTES`).
pub const KIT_RECORD_BYTES: usize = 16 * 1024;
/// Largest starting Canvas, in UTF-8 bytes (`CANVAS_BYTES`).
pub const CANVAS_BYTES: usize = 24 * 1024;
/// A full catalog read; reaching it fails rather than show a partial one.
pub const CATALOG_LIMIT: usize = 500;
/// Ciphertext and plaintext bounds of the native decoder.
const CIPHERTEXT_BYTES: usize = 64 * 1024;
const PLAINTEXT_BYTES: usize = 48 * 1024;

const INVALID: &str = "Invalid channel recipe";
pub const ME_CAPACITY: &str =
    "Me storage is full. Move a conversation from Me to Messages, then try again.";
const TOO_LARGE: &str = "This saved recipe exceeds 16 KiB; shorten its Canvas or selections";

/// Channels, teams and agents a template copies into a new channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lineup {
    pub team_ids: Vec<String>,
    pub agents: Vec<String>,
    pub canvas: String,
}

impl Lineup {
    pub fn is_empty(&self) -> bool {
        self.team_ids.is_empty() && self.agents.is_empty() && self.canvas.is_empty()
    }
}

/// A saved team: agents reused together. Version 2 teams carry a
/// portable manifest whose definitions live in payload chunks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    pub agents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portable: Option<TeamManifest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Template {
    pub id: String,
    pub name: String,
    pub description: String,
    pub team_ids: Vec<String>,
    pub agents: Vec<String>,
    pub canvas: String,
}

impl Template {
    pub fn lineup(&self) -> Lineup {
        Lineup {
            team_ids: self.team_ids.clone(),
            agents: self.agents.clone(),
            canvas: self.canvas.clone(),
        }
    }

    pub fn set_lineup(&mut self, lineup: Lineup) {
        self.team_ids = lineup.team_ids;
        self.agents = lineup.agents;
        self.canvas = lineup.canvas;
    }

    /// A blank template with a fresh id.
    pub fn blank() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            description: String::new(),
            team_ids: Vec::new(),
            agents: Vec::new(),
            canvas: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub name: String,
    pub default_template_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupsId {
    /// Me's groups and placement, the only groups record since personal
    /// groups were retired.
    Me,
}

impl GroupsId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Me => "me",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Groups {
    pub id: GroupsId,
    pub groups: Vec<Group>,
    pub assignments: BTreeMap<String, String>,
    /// Me only: placement independent of group membership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum KitValue {
    Team(Team),
    Template(Template),
    Groups(Groups),
}

impl KitValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Team(_) => "team",
            Self::Template(_) => "template",
            Self::Groups(_) => "groups",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Team(team) => &team.id,
            Self::Template(template) => &template.id,
            Self::Groups(groups) => groups.id.as_str(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Team(team) => &team.name,
            Self::Template(template) => &template.name,
            Self::Groups(_) => "",
        }
    }
}

/// One decoded recipe: `{version, community, deleted, value}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KitRecord {
    pub version: u8,
    pub community: String,
    pub deleted: bool,
    pub value: KitValue,
}

impl KitRecord {
    /// The record to save `value` as: version 2 for portable teams.
    pub fn new(value: KitValue, community: &str, deleted: bool) -> Self {
        let version = match &value {
            KitValue::Team(team) if team.portable.is_some() => 2,
            _ => 1,
        };
        Self {
            version,
            community: community.to_owned(),
            deleted,
            value,
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("records serialize")
    }

    /// The `d` coordinate (`coordinate`).
    pub fn coordinate(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.tag(),
            encode_component(&self.community),
            self.value.type_name(),
            self.value.id()
        )
    }

    /// The discovery `t` tag (`kitTag`).
    pub fn tag(&self) -> &'static str {
        match &self.value {
            KitValue::Groups(groups) if groups.id == GroupsId::Me => ME_KIT_TAG,
            KitValue::Template(template) if template.id.starts_with("me-section-") => ME_KIT_TAG,
            _ if self.version == 2 => MANIFEST_TAG,
            _ => KIT_TAG,
        }
    }

    /// Whether it fits the 16 KiB record budget (`kitRecordFits`).
    pub fn fits(&self) -> bool {
        serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= KIT_RECORD_BYTES)
    }
}

/// A recipe as last read, with the event that carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KitEntry {
    pub record: KitRecord,
    pub event_id: EventId,
    pub created_at: u64,
}

/// An agent a recipe can name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentChoice {
    pub pubkey: String,
    pub name: String,
    pub avatar: Option<String>,
}

/// `encodeURIComponent` of a community origin.
pub fn encode_component(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `^[a-zA-Z0-9_-]{1,128}$`.
pub fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn object(value: Option<&Value>) -> Result<&Map<String, Value>, String> {
    value
        .and_then(Value::as_object)
        .ok_or_else(|| INVALID.into())
}

fn text(value: Option<&Value>, max: usize, nonempty: bool) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if utf16_len(text) <= max && (!nonempty || !text.trim().is_empty()) => {
            Ok(text.to_owned())
        }
        _ => Err("Invalid channel recipe text".into()),
    }
}

fn id(value: Option<&Value>) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if identifier(text) => Ok(text.to_owned()),
        _ => Err("Invalid channel recipe ID".into()),
    }
}

fn keys(value: Option<&Value>, valid: fn(&str) -> bool, max: usize) -> Result<Vec<String>, String> {
    let invalid = || "Invalid channel recipe selection".to_owned();
    let list = value.and_then(Value::as_array).ok_or_else(invalid)?;
    if list.len() > max {
        return Err(invalid());
    }
    let mut out: Vec<String> = Vec::with_capacity(list.len());
    for item in list {
        let key = item.as_str().filter(|key| valid(key)).ok_or_else(invalid)?;
        if out.iter().any(|seen| seen == key) {
            return Err(invalid());
        }
        out.push(key.to_owned());
    }
    Ok(out)
}

/// `parseLineup`: the selections and Canvas of a template or draft.
pub fn parse_lineup(value: &Value) -> Result<Lineup, String> {
    let v = object(Some(value))?;
    let canvas = text(v.get("canvas"), CANVAS_BYTES, false)?;
    if canvas.len() > CANVAS_BYTES {
        return Err("Canvas is too large (24 KiB maximum)".into());
    }
    Ok(Lineup {
        team_ids: keys(v.get("teamIds"), identifier, 100)?,
        agents: keys(v.get("agents"), is_key, 200)?,
        canvas,
    })
}

/// `parseKitRecord`: the TypeScript model checks with their messages, then
/// the native admission rules, then the 16 KiB budget.
pub fn parse_kit_record(raw: &Value, community: &str) -> Result<KitRecord, String> {
    let r = object(Some(raw))?;
    let v = object(r.get("value"))?;
    let version = match r.get("version").and_then(Value::as_u64) {
        Some(version @ (1 | 2)) if r["version"].is_u64() => version as u8,
        _ => return Err("Unsupported recipe or wrong community".into()),
    };
    let deleted = match r.get("deleted") {
        Some(Value::Bool(deleted))
            if r.get("community").and_then(Value::as_str) == Some(community) =>
        {
            *deleted
        }
        _ => return Err("Unsupported recipe or wrong community".into()),
    };
    let kind = v.get("type").and_then(Value::as_str);
    let value = match kind {
        Some("team") => {
            let portable = if version == 2 {
                Some(payload::parse_team_manifest(
                    v.get("portable").unwrap_or(&Value::Null),
                )?)
            } else {
                None
            };
            KitValue::Team(Team {
                id: id(v.get("id"))?,
                name: text(v.get("name"), if version == 2 { 256 } else { 120 }, true)?,
                agents: keys(v.get("agents"), is_key, if version == 2 { 32 } else { 200 })?,
                portable,
            })
        }
        Some("template") => {
            let lineup = parse_lineup(&Value::Object(v.clone()))?;
            KitValue::Template(Template {
                id: id(v.get("id"))?,
                name: text(v.get("name"), 120, true)?,
                description: text(v.get("description"), 1000, false)?,
                team_ids: lineup.team_ids,
                agents: lineup.agents,
                canvas: lineup.canvas,
            })
        }
        Some("groups") => {
            let groups_id = match v.get("id").and_then(Value::as_str) {
                Some("me") => GroupsId::Me,
                _ => return Err("Unsupported channel recipe type".into()),
            };
            let list = v
                .get("groups")
                .and_then(Value::as_array)
                .filter(|list| list.len() <= 100)
                .ok_or("Too many Me groups")?;
            let mut groups: Vec<Group> = Vec::with_capacity(list.len());
            for raw in list {
                let g = object(Some(raw))?;
                let default = match g.get("defaultTemplateId").and_then(Value::as_str) {
                    Some("") => String::new(),
                    _ => id(g.get("defaultTemplateId"))?,
                };
                let group = Group {
                    id: id(g.get("id"))?,
                    name: text(g.get("name"), 120, true)?,
                    default_template_id: default,
                };
                if groups.iter().any(|seen| seen.id == group.id) {
                    return Err("Duplicate Me group".into());
                }
                groups.push(group);
            }
            let assignments = object(v.get("assignments"))?;
            let placed = |(channel, group): (&String, &Value)| {
                channel.len() == 36
                    && channel
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'-'))
                    && group
                        .as_str()
                        .is_some_and(|group| groups.iter().any(|g| g.id == group))
            };
            if assignments.len() > 1000 || !assignments.iter().all(placed) {
                return Err("Invalid Me group placement".into());
            }
            let channels = Some(keys(
                Some(v.get("channels").unwrap_or(&json!([]))),
                is_uuid,
                1000,
            )?);
            KitValue::Groups(Groups {
                id: groups_id,
                groups,
                assignments: assignments
                    .iter()
                    .map(|(c, g)| (c.clone(), g.as_str().unwrap_or_default().to_owned()))
                    .collect(),
                channels,
            })
        }
        _ => return Err("Unsupported channel recipe type".into()),
    };
    match &value {
        KitValue::Team(team)
            if version == 2 && (team.portable.is_none() || team.agents.is_empty()) =>
        {
            return Err("Unsupported portable recipe".into());
        }
        KitValue::Template(_) | KitValue::Groups(_) if version == 2 => {
            return Err("Unsupported portable recipe".into());
        }
        _ => {}
    }
    if version == 1 && v.contains_key("portable") {
        return Err("Portable teams require version 2".into());
    }
    let record = KitRecord {
        version,
        community: community.to_owned(),
        deleted,
        value,
    };
    if !record.fits() {
        return Err(match &record.value {
            KitValue::Groups(groups) if groups.id == GroupsId::Me => ME_CAPACITY.into(),
            _ => TOO_LARGE.into(),
        });
    }
    // The native boundary: exact fields and the coordinate it must use.
    let coordinate = native::coordinate(&record.to_json(), community)?;
    if coordinate != record.coordinate() {
        return Err(INVALID.into());
    }
    Ok(record)
}

/// A team's shared instructions head (`TextRecord`): a manifest of the
/// text's chunks, or a tombstone. It is kept apart from the team record,
/// so builds that only read team records never see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRecord {
    pub community: String,
    pub deleted: bool,
    /// The team's ID.
    pub id: String,
    pub owner: String,
    /// `None` exactly when deleted.
    pub manifest: Option<TeamManifest>,
}

impl TextRecord {
    /// A live head for `manifest`, or a tombstone without one.
    pub fn new(community: &str, id: &str, owner: &str, manifest: Option<TeamManifest>) -> Self {
        Self {
            community: community.to_owned(),
            deleted: manifest.is_none(),
            id: id.to_owned(),
            owner: owner.to_owned(),
            manifest,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "version": 1,
            "community": self.community,
            "deleted": self.deleted,
            "value": {
                "type": "team-text",
                "id": self.id,
                "owner": self.owner,
                "manifest": self.manifest,
            },
        })
    }

    pub fn coordinate(&self) -> String {
        text_coordinate(&self.community, &self.owner, &self.id)
    }
}

/// The `d` coordinate of a team's text head (`textCoordinate`).
pub fn text_coordinate(community: &str, owner: &str, team_id: &str) -> String {
    format!(
        "{TEXT_TAG}:{}:{owner}:{team_id}",
        encode_component(community)
    )
}

/// `parseTextRecord`: exact fields, a live head's manifest owned by the
/// record's owner, a tombstone without one, within the record budget.
pub fn parse_text_record(raw: &Value, community: &str) -> Result<TextRecord, String> {
    const INVALID_TEXT: &str = "Invalid team instructions record";
    let r = raw.as_object().ok_or(INVALID_TEXT)?;
    let v = r
        .get("value")
        .and_then(Value::as_object)
        .ok_or(INVALID_TEXT)?;
    let deleted = r.get("deleted").and_then(Value::as_bool);
    let owner = v.get("owner").and_then(Value::as_str).filter(|o| is_key(o));
    let (Some(deleted), Some(owner)) = (deleted, owner) else {
        return Err(INVALID_TEXT.into());
    };
    if r.keys()
        .any(|key| !["version", "community", "deleted", "value"].contains(&key.as_str()))
        || v.keys()
            .any(|key| !["type", "id", "owner", "manifest"].contains(&key.as_str()))
        || r.get("version") != Some(&json!(1))
        || r.get("community").and_then(Value::as_str) != Some(community)
        || v.get("type").and_then(Value::as_str) != Some("team-text")
    {
        return Err(INVALID_TEXT.into());
    }
    let manifest = match (deleted, v.get("manifest").unwrap_or(&Value::Null)) {
        (true, Value::Null) => None,
        (true, _) => return Err(INVALID_TEXT.into()),
        (false, raw) => {
            let manifest = payload::parse_text_manifest(raw)?;
            if manifest.owner != owner {
                return Err(INVALID_TEXT.into());
            }
            Some(manifest)
        }
    };
    let record = TextRecord {
        community: community.to_owned(),
        deleted,
        id: id(v.get("id"))?,
        owner: owner.to_owned(),
        manifest,
    };
    if serde_json::to_vec(&record.to_json()).map_or(true, |bytes| bytes.len() > KIT_RECORD_BYTES) {
        return Err(INVALID_TEXT.into());
    }
    Ok(record)
}

/// A decoded private record: a recipe, a portable team chunk, or a team's
/// text head.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrivateRecord {
    Kit(KitRecord),
    Payload(TeamPayload),
    Text(TextRecord),
}

/// `parsePrivateRecord`.
#[cfg(test)]
pub fn parse_private_record(raw: &Value, community: &str) -> Result<PrivateRecord, String> {
    match raw
        .get("value")
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str)
    {
        Some("team-payload") => {
            payload::parse_payload_record(raw, community).map(PrivateRecord::Payload)
        }
        Some("team-text") => parse_text_record(raw, community).map(PrivateRecord::Text),
        _ => parse_kit_record(raw, community).map(PrivateRecord::Kit),
    }
}

/// The kit cipher's write side (`relay_kit_prepare` / `kit_cipher`): only
/// a record the native rules accept is ever encrypted, and only to the
/// viewer.
pub fn seal(identity: &Identity, raw: &Value, community: &str) -> Result<String, String> {
    native::prepare(raw, community)?;
    let text = serde_json::to_string(raw).map_err(|_| INVALID.to_owned())?;
    identity
        .encrypt_to_self(&text)
        .map_err(|_| INVALID.to_owned())
}

/// The kit cipher's read side (`relay_kit_decode`): the viewer's own 30078
/// event, decrypted and admitted against its tags. Portable records and
/// team text heads must belong to the viewer.
pub fn open(identity: &Identity, event: &Verified, community: &str) -> Result<Value, String> {
    let viewer = identity.pubkey_hex();
    if event.kind_u16() != kinds::APP_DATA
        || event.pubkey_hex() != viewer
        || event.content.len() > CIPHERTEXT_BYTES
    {
        return Err(INVALID.into());
    }
    let plaintext = identity
        .decrypt_from_self(event.event())
        .map_err(|_| INVALID.to_owned())?;
    if plaintext.len() > PLAINTEXT_BYTES {
        return Err(INVALID.into());
    }
    let raw: Value = serde_json::from_str(&plaintext).map_err(|_| INVALID.to_owned())?;
    let event_json = serde_json::to_value(event.event()).map_err(|_| INVALID.to_owned())?;
    native::admission(&event_json, &raw, community)?;
    let owner = if matches!(
        raw["value"]["type"].as_str(),
        Some("team-payload" | "team-text")
    ) {
        Some(&raw["value"]["owner"])
    } else if raw["version"] == 2 {
        Some(&raw["value"]["portable"]["owner"])
    } else {
        None
    };
    if owner.is_some_and(|owner| owner.as_str() != Some(viewer.as_str())) {
        return Err("Portable team belongs to another viewer".into());
    }
    Ok(raw)
}

/// The 30078 draft that saves `record`.
#[cfg(test)]
pub fn draft(identity: &Identity, record: &KitRecord) -> Result<crate::messages::Draft, String> {
    let raw = record.to_json();
    Ok(crate::messages::Draft {
        kind: kinds::APP_DATA,
        content: seal(identity, &raw, &record.community)?,
        tags: vec![
            vec!["d".into(), record.coordinate()],
            vec!["t".into(), record.tag().into()],
        ],
    })
}

/// Every recipe of the viewer, across communities (`refresh`).
pub fn catalog_filter(viewer: &str) -> Filter {
    filter(json!({
        "kinds": [kinds::APP_DATA],
        "authors": [viewer],
        "#t": [KIT_TAG, MANIFEST_TAG, ME_KIT_TAG],
        "limit": CATALOG_LIMIT,
    }))
}

/// The head of one coordinate.
pub fn record_filter(viewer: &str, coordinate: &str, strong: bool) -> Filter {
    let mut f = filter(json!({
        "kinds": [kinds::APP_DATA],
        "authors": [viewer],
        "#d": [coordinate],
        "limit": if strong { 2 } else { 1 },
    }));
    if strong {
        f.insert("consistency".into(), json!("strong"));
    }
    f
}

/// One event by id, strongly consistent (`confirm`).
pub fn id_filter(id: &EventId) -> Filter {
    filter(json!({"ids": [id.to_hex()], "limit": 1, "consistency": "strong"}))
}

/// `selectedHead`: newest, then lowest id.
pub fn selected_head<'a>(events: impl IntoIterator<Item = &'a Verified>) -> Option<&'a Verified> {
    events.into_iter().reduce(|best, event| {
        if crate::event::is_newer(event, best) {
            event
        } else {
            best
        }
    })
}

/// The head event of each coordinate in this community, from a catalog
/// read. Other communities' and malformed coordinates are skipped.
pub fn catalog_heads<'a>(events: &'a [Verified], community: &str) -> Vec<&'a Verified> {
    let encoded = encode_component(community);
    let prefixes: Vec<String> = [KIT_TAG, MANIFEST_TAG, ME_KIT_TAG]
        .iter()
        .map(|tag| format!("{tag}:{encoded}:"))
        .collect();
    let retired = format!("{KIT_TAG}:{encoded}:groups:personal");
    let mut heads: BTreeMap<&str, &Verified> = BTreeMap::new();
    for event in events {
        let Some(d) = event.tag("d") else { continue };
        // Retired personal groups: ignore saved records, never reject the
        // catalog.
        if !prefixes.iter().any(|prefix| d.starts_with(prefix.as_str())) || d == retired {
            continue;
        }
        match heads.get(d) {
            Some(old) if !crate::event::is_newer(event, old) => {}
            _ => {
                heads.insert(d, event);
            }
        }
    }
    heads.into_values().collect()
}

/// Decodes one catalog head into an entry, checking its coordinate.
#[cfg(test)]
pub fn decode_entry(
    identity: &Identity,
    event: &Verified,
    community: &str,
) -> Result<KitEntry, String> {
    let raw = open(identity, event, community)?;
    let record = parse_kit_record(&raw, community)?;
    if event.tag("d") != Some(record.coordinate().as_str()) {
        return Err("Recipe decode mismatch".into());
    }
    Ok(KitEntry {
        record,
        event_id: event.id,
        created_at: event.created_at_secs(),
    })
}

/// The live (not deleted) templates in catalog order.
pub fn templates(entries: &[KitEntry]) -> Vec<&Template> {
    entries
        .iter()
        .filter(|e| !e.record.deleted)
        .filter_map(|e| match &e.record.value {
            KitValue::Template(template) => Some(template),
            _ => None,
        })
        .collect()
}

/// The live teams in catalog order.
pub fn teams(entries: &[KitEntry]) -> Vec<&Team> {
    entries
        .iter()
        .filter(|e| !e.record.deleted)
        .filter_map(|e| match &e.record.value {
            KitValue::Team(team) => Some(team),
            _ => None,
        })
        .collect()
}

/// `resolveLineup`: the distinct agents of the selected teams and agents,
/// each still available here.
pub fn resolve_lineup(
    lineup: &Lineup,
    entries: &[KitEntry],
    agents: &[AgentChoice],
) -> Result<Vec<AgentChoice>, String> {
    let mut selected: Vec<String> = Vec::new();
    let mut add = |key: &String| {
        if !selected.contains(key) {
            selected.push(key.clone());
        }
    };
    lineup.agents.iter().for_each(&mut add);
    for team_id in &lineup.team_ids {
        let team = entries.iter().find_map(|e| match &e.record.value {
            KitValue::Team(team) if team.id == *team_id && !e.record.deleted => Some(team),
            _ => None,
        });
        let Some(team) = team else {
            return Err(format!(
                "A selected team is unavailable ({team_id}); remove or replace it"
            ));
        };
        team.agents.iter().for_each(&mut add);
    }
    if selected.len() > 200 {
        return Err("Choose at most 200 distinct agents".into());
    }
    selected
        .iter()
        .map(|pubkey| {
            agents
                .iter()
                .find(|agent| agent.pubkey == *pubkey)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "Agent {} is unavailable in this community; remove or replace it",
                        npub(pubkey)
                    )
                })
        })
        .collect()
}

pub(crate) fn npub(pubkey: &str) -> String {
    use nostr::nips::nip19::ToBech32;
    nostr::prelude::PublicKey::from_hex(pubkey)
        .ok()
        .and_then(|key| key.to_bech32().ok())
        .unwrap_or_else(|| pubkey.to_owned())
}

#[cfg(test)]
mod tests;
