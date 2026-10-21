//! Portable team payloads (`features/channel-templates/team-payload.ts`):
//! the manifest and 16 KiB chunk records of a team snapshot (up to
//! 8 MiB) or of a team's shared instructions (`v1:` and their UTF-8
//! bytes). The app no longer writes or assembles them; it only admits
//! such records when reading the kit.

use super::identifier;
use crate::discovery::is_uuid;
use crate::members::is_key;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const TEAM_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
pub const TEAM_CHUNK_BYTES: usize = 16 * 1024;
pub const TEAM_CHUNKS: usize = TEAM_PAYLOAD_BYTES / TEAM_CHUNK_BYTES;

const INVALID_MANIFEST: &str = "Invalid portable team manifest";
const INVALID_PAYLOAD: &str = "Invalid portable team payload";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamManifest {
    pub version: u8,
    pub owner: String,
    pub revision: String,
    pub digest: String,
    pub bytes: u64,
    pub chunks: u64,
}

/// One chunk of a portable team.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamPayload {
    pub version: u8,
    pub community: String,
    pub owner: String,
    pub team_id: String,
    pub revision: String,
    pub index: u64,
    pub data: String,
}

impl TeamPayload {
    /// The record that stores this chunk (`PayloadRecord`).
    pub fn record(&self) -> Value {
        let mut value = serde_json::to_value(self).expect("payloads serialize");
        value["type"] = json!("team-payload");
        value["id"] = json!(format!("{}-{}", self.revision, self.index));
        json!({"version": 1, "community": self.community, "deleted": false, "value": value})
    }

    pub fn coordinate(&self) -> String {
        payload_coordinate(
            &self.community,
            &self.owner,
            &self.team_id,
            &self.revision,
            self.index,
        )
    }
}

/// `parseTeamManifest`.
pub fn parse_team_manifest(raw: &Value) -> Result<TeamManifest, String> {
    let bytes = raw["bytes"].as_u64().unwrap_or(0);
    let valid = raw["version"] == 1
        && raw["owner"].as_str().is_some_and(is_key)
        && raw["revision"].as_str().is_some_and(is_uuid)
        && raw["digest"].as_str().is_some_and(is_key)
        && (1..=TEAM_PAYLOAD_BYTES as u64).contains(&bytes)
        && raw["chunks"].as_u64() == Some(bytes.div_ceil(TEAM_CHUNK_BYTES as u64));
    if !valid {
        return Err(INVALID_MANIFEST.into());
    }
    Ok(TeamManifest {
        version: 1,
        owner: raw["owner"].as_str().unwrap_or_default().to_owned(),
        revision: raw["revision"].as_str().unwrap_or_default().to_owned(),
        digest: raw["digest"].as_str().unwrap_or_default().to_owned(),
        bytes,
        chunks: bytes.div_ceil(TEAM_CHUNK_BYTES as u64),
    })
}

/// `parseTeamPayload`.
pub fn parse_team_payload(raw: &Value, community: &str) -> Result<TeamPayload, String> {
    let index = raw["index"].as_u64();
    let data = raw["data"].as_str().unwrap_or_default();
    let valid = raw["version"] == 1
        && raw["community"].as_str() == Some(community)
        && raw["owner"].as_str().is_some_and(is_key)
        && raw["teamId"].as_str().is_some_and(identifier)
        && raw["revision"].as_str().is_some_and(is_uuid)
        && index.is_some_and(|index| index < TEAM_CHUNKS as u64)
        && raw["data"].is_string()
        && data.len() <= TEAM_CHUNK_BYTES.div_ceil(3) * 4
        && STANDARD
            .decode(data)
            .is_ok_and(|bytes| STANDARD.encode(bytes) == data);
    if !valid {
        return Err(INVALID_PAYLOAD.into());
    }
    Ok(TeamPayload {
        version: 1,
        community: community.to_owned(),
        owner: raw["owner"].as_str().unwrap_or_default().to_owned(),
        team_id: raw["teamId"].as_str().unwrap_or_default().to_owned(),
        revision: raw["revision"].as_str().unwrap_or_default().to_owned(),
        index: index.unwrap_or_default(),
        data: data.to_owned(),
    })
}

/// `parsePayloadRecord`: the chunk record, its id bound to its position.
pub fn parse_payload_record(raw: &Value, community: &str) -> Result<TeamPayload, String> {
    let value = &raw["value"];
    if raw["version"] != 1
        || raw["community"].as_str() != Some(community)
        || raw["deleted"] != false
        || value["type"] != "team-payload"
    {
        return Err("Invalid team payload record".into());
    }
    let payload = parse_team_payload(value, community)?;
    if value["id"].as_str() != Some(format!("{}-{}", payload.revision, payload.index).as_str()) {
        return Err("Invalid team payload ID".into());
    }
    // The native boundary accepts exactly these fields.
    buzz_desktop::relay::kit::coordinate(raw, community)?;
    Ok(payload)
}

pub fn payload_coordinate(
    community: &str,
    owner: &str,
    team_id: &str,
    revision: &str,
    index: u64,
) -> String {
    format!(
        "{}:{}:{owner}:{team_id}:{revision}:{index}",
        super::PAYLOAD_TAG,
        super::encode_component(community)
    )
}

/// Largest team instructions, in UTF-8 bytes (`TEAM_TEXT_BYTES`). They
/// travel as ASCII `v1:` plus their raw UTF-8 bytes.
pub const TEAM_TEXT_BYTES: usize = 128 * 1024;
const TEXT_PREFIX: &str = "v1:";

/// `parseTextManifest`: a team manifest with exactly its six fields, as
/// native admission requires, sized for the prefix and 128 KiB of text.
pub fn parse_text_manifest(raw: &Value) -> Result<TeamManifest, String> {
    let manifest = parse_team_manifest(raw)?;
    if raw.as_object().map_or(0, |fields| fields.len()) != 6
        || manifest.bytes < TEXT_PREFIX.len() as u64
        || manifest.bytes > (TEXT_PREFIX.len() + TEAM_TEXT_BYTES) as u64
    {
        return Err("Invalid team instructions manifest".into());
    }
    Ok(manifest)
}
