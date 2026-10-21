//! The Me group record of the recipe catalog (`relay/me-preferences.ts`):
//! the Me page's groups and which conversations are placed in Me.
//!
//! The catalog itself is read once, by the session's recipe reader, with
//! [`super::catalog_filter`] and [`decode`]; these entries keep each
//! record's raw JSON for the Me settings that edit it in place.

use super::identifier;
use crate::discovery::is_uuid;
use crate::event::Verified;
use crate::sidebar::{MeGroup, MeGroups};
use buzz_desktop::relay::kit as native;
use buzz_identity::Identity;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub use super::{
    CANVAS_BYTES, CATALOG_LIMIT, GroupsId, KIT_RECORD_BYTES as RECORD_BYTES, ME_CAPACITY,
};
pub use native::{KIT_TAG, MANIFEST_TAG, ME_KIT_TAG, PAYLOAD_TAG};

/// `removedSectionMessage` (`sessions/workspace.ts`).
pub const REMOVED_SECTION: &str = "The section was removed. Continue without a section to retry.";

/// One decoded record, as the relay's head of its coordinate, with its
/// raw JSON; [`super::KitEntry`] is the parsed form.
#[derive(Clone, Debug, PartialEq)]
pub struct RawKitEntry {
    /// `{version, community, deleted, value}`, validated.
    pub record: Value,
    pub event_id: String,
    pub created_at: u64,
}

impl RawKitEntry {
    pub fn kind(&self) -> &str {
        self.record["value"]["type"].as_str().unwrap_or_default()
    }

    pub fn id(&self) -> &str {
        self.record["value"]["id"].as_str().unwrap_or_default()
    }

    pub fn deleted(&self) -> bool {
        self.record["deleted"] == true
    }

    /// The groups value of a live `groups` record.
    pub fn groups(&self) -> Option<MeGroups> {
        (!self.deleted() && self.kind() == "groups")
            .then(|| parse_groups(&self.record["value"]).ok())
            .flatten()
    }
}

/// The `groups` record with this id, deleted or not (`meGroups`).
pub fn groups_entry(entries: &[RawKitEntry], id: GroupsId) -> Option<&RawKitEntry> {
    entries
        .iter()
        .find(|entry| entry.kind() == "groups" && entry.id() == id.as_str())
}

/// The live groups of that record, if any.
pub fn groups(entries: &[RawKitEntry], id: GroupsId) -> Option<MeGroups> {
    groups_entry(entries, id).and_then(RawKitEntry::groups)
}

/// A live template by id (`templateEntry`).
pub fn template<'a>(entries: &'a [RawKitEntry], id: &str) -> Option<&'a RawKitEntry> {
    entries
        .iter()
        .find(|entry| !entry.deleted() && entry.kind() == "template" && entry.id() == id)
}

/// The `groups` value of a record (`parseKitRecord`).
pub fn parse_groups(value: &Value) -> Result<MeGroups, String> {
    let invalid = || "Invalid channel recipe".to_owned();
    let list = value["groups"].as_array().ok_or_else(invalid)?;
    if list.len() > 100 {
        return Err("Too many Me groups".into());
    }
    let mut groups = Vec::with_capacity(list.len());
    for raw in list {
        let id = raw["id"].as_str().filter(|id| identifier(id));
        let name = raw["name"]
            .as_str()
            .filter(|name| name.encode_utf16().count() <= 120 && !name.trim().is_empty());
        let default = raw["defaultTemplateId"]
            .as_str()
            .filter(|id| id.is_empty() || identifier(id));
        let (Some(id), Some(name), Some(default)) = (id, name, default) else {
            return Err(invalid());
        };
        if groups.iter().any(|group: &MeGroup| group.id == id) {
            return Err("Duplicate Me group".into());
        }
        groups.push(MeGroup {
            id: id.into(),
            name: name.into(),
            default_template_id: default.into(),
        });
    }
    let raw = value["assignments"].as_object().ok_or_else(invalid)?;
    let mut assignments = BTreeMap::new();
    for (channel, group) in raw {
        let group = group.as_str().unwrap_or_default();
        if raw.len() > 1000 || !is_uuid(channel) || !groups.iter().any(|g| g.id == group) {
            return Err("Invalid Me group placement".into());
        }
        assignments.insert(channel.clone(), group.to_owned());
    }
    let channels = match value.get("channels") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) => {
            let mut channels: Vec<String> = Vec::with_capacity(list.len());
            for id in list {
                match id.as_str() {
                    Some(id) if is_uuid(id) && !channels.iter().any(|c| c == id) => {
                        channels.push(id.into())
                    }
                    _ => return Err("Invalid channel recipe selection".into()),
                }
            }
            if channels.len() > 1000 {
                return Err("Invalid channel recipe selection".into());
            }
            channels
        }
        Some(_) => return Err("Invalid channel recipe selection".into()),
    };
    Ok(MeGroups {
        groups,
        assignments,
        channels,
    })
}

/// The `groups` value of a record.
pub fn groups_value(id: GroupsId, groups: &MeGroups) -> Value {
    let value = json!({
        "type": "groups",
        "id": id.as_str(),
        "groups": groups.groups.iter().map(|group| json!({
            "id": group.id,
            "name": group.name,
            "defaultTemplateId": group.default_template_id,
        })).collect::<Vec<_>>(),
        "assignments": groups.assignments.iter()
            .map(|(channel, group)| (channel.clone(), json!(group)))
            .collect::<Map<_, _>>(),
        "channels": groups.channels,
    });
    value
}

/// The full record of `value` (`{version, community, deleted, value}`).
pub fn record(community: &str, value: Value, deleted: bool) -> Value {
    let version = if value["type"] == "team" && value.get("portable").is_some() {
        2
    } else {
        1
    };
    json!({
        "version": version,
        "community": community,
        "deleted": deleted,
        "value": value,
    })
}

/// `kitRecordFits`: the record's JSON is at most 16 KiB.
pub fn fits(record: &Value) -> bool {
    serde_json::to_vec(record).is_ok_and(|bytes| bytes.len() <= RECORD_BYTES)
}

/// The `d` coordinate of a record, after validating it for `community`.
pub fn coordinate(record: &Value, community: &str) -> Result<String, String> {
    native::coordinate(record, community)
}

/// The `t` tag a record is filed under (`kitTag`).
pub fn tag(record: &Value) -> &'static str {
    if record["value"]["type"] == "team-payload" {
        PAYLOAD_TAG
    } else if record["value"]["type"] == "team-text" {
        super::TEXT_TAG
    } else if record["version"] == 2 {
        MANIFEST_TAG
    } else if (record["value"]["type"] == "groups" && record["value"]["id"] == "me")
        || (record["value"]["type"] == "template"
            && record["value"]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("me-section-")))
    {
        ME_KIT_TAG
    } else {
        KIT_TAG
    }
}

/// Opens one catalog head (`relay_kit_decode`) with the kit cipher, and
/// checks a groups record's value as `parseKitRecord` does.
pub fn decode(
    identity: &Identity,
    event: &Verified,
    community: &str,
) -> Result<RawKitEntry, String> {
    let raw = super::open(identity, event, community)?;
    if raw["value"]["type"] == "groups" {
        parse_groups(&raw["value"])?;
    }
    Ok(RawKitEntry {
        record: raw,
        event_id: event.id_hex(),
        created_at: event.created_at_secs(),
    })
}

/// Encrypts a record for saving (`relay_kit_prepare`), with its tags.
pub fn prepare(
    identity: &Identity,
    record: &Value,
    community: &str,
) -> Result<(String, Vec<Vec<String>>), String> {
    let coordinate = coordinate(record, community)?;
    if !fits(record) {
        return Err(
            if record["value"]["type"] == "groups" && record["value"]["id"] == "me" {
                ME_CAPACITY.into()
            } else {
                "This saved recipe exceeds 16 KiB; shorten its Canvas or selections".into()
            },
        );
    }
    // Read back with `parseKitRecord`, which counts text in UTF-16 units
    // where the native boundary counts characters.
    if [KIT_TAG, MANIFEST_TAG, ME_KIT_TAG].contains(&tag(record)) {
        super::parse_kit_record(record, community)?;
    }
    let content = super::seal(identity, record, community)?;
    Ok((
        content,
        vec![
            vec!["d".into(), coordinate],
            vec!["t".into(), tag(record).into()],
        ],
    ))
}

/// Me placement after putting `id` in (or taking it out of) Me
/// (`me-preferences.ts` `placed`).
pub fn placed(
    value: &MeGroups,
    id: &str,
    personal: bool,
    section: Option<&str>,
) -> Result<MeGroups, String> {
    if let Some(section) = section
        && !value.groups.iter().any(|group| group.id == section)
    {
        return Err(REMOVED_SECTION.into());
    }
    let mut next = value.clone();
    if !personal {
        next.assignments.remove(id);
        next.channels.retain(|channel| channel != id);
    } else {
        if let Some(section) = section {
            next.assignments.insert(id.into(), section.into());
        }
        if !next.channels.iter().any(|channel| channel == id) {
            next.channels.push(id.into());
        }
    }
    Ok(next)
}

/// `admit`: with a saved record, the next value must still fit its
/// envelope; with none, one placement always fits.
pub fn admit(value: &MeGroups, entry: Option<&RawKitEntry>, community: &str) -> Result<(), String> {
    if entry.is_some() && !fits(&record(community, groups_value(GroupsId::Me, value), false)) {
        return Err(ME_CAPACITY.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    const COMMUNITY: &str = "https://community.test";
    const CHANNEL: &str = "11111111-1111-4111-8111-111111111111";

    fn me(groups: &[(&str, &str)], channels: &[&str]) -> MeGroups {
        MeGroups {
            groups: groups
                .iter()
                .map(|(id, name)| MeGroup {
                    id: (*id).into(),
                    name: (*name).into(),
                    default_template_id: String::new(),
                })
                .collect(),
            assignments: BTreeMap::new(),
            channels: channels.iter().map(|c| (*c).into()).collect(),
        }
    }

    #[test]
    fn me_groups_use_the_me_tag_and_validate() {
        let value = groups_value(GroupsId::Me, &me(&[("g1", "Work")], &[CHANNEL]));
        let record = record(COMMUNITY, value, false);
        let d = coordinate(&record, COMMUNITY).unwrap();
        assert!(d.starts_with(&format!(
            "{ME_KIT_TAG}:https%3A%2F%2Fcommunity.test:groups:me"
        )));
        assert_eq!(tag(&record), ME_KIT_TAG);
        let parsed = parse_groups(&record["value"]).unwrap();
        assert_eq!(parsed.channels, [CHANNEL]);
    }

    #[test]
    fn placement_adds_and_removes_with_sections() {
        let value = me(&[("g1", "Work")], &[]);
        let next = placed(&value, CHANNEL, true, Some("g1")).unwrap();
        assert_eq!(next.channels, [CHANNEL]);
        assert_eq!(next.assignments[CHANNEL], "g1");
        let back = placed(&next, CHANNEL, false, None).unwrap();
        assert!(back.channels.is_empty() && back.assignments.is_empty());
        assert_eq!(
            placed(&value, CHANNEL, true, Some("gone")).unwrap_err(),
            REMOVED_SECTION
        );
    }

    #[test]
    fn names_count_utf16_units_as_the_reader_does() {
        // 61 characters, 122 UTF-16 units.
        let name = "\u{1F600}".repeat(61);
        let value = groups_value(GroupsId::Me, &me(&[("g1", &name)], &[]));
        assert!(parse_groups(&value).is_err());
        let record = record(COMMUNITY, value, false);
        assert!(
            coordinate(&record, COMMUNITY).is_ok(),
            "native counts chars"
        );
        assert!(prepare(&Identity::ephemeral(), &record, COMMUNITY).is_err());
    }

    #[test]
    fn tags_follow_the_record_shape() {
        let text = json!({"version": 1, "value": {"type": "team-text"}});
        assert_eq!(tag(&text), crate::kit::TEXT_TAG);
        // Not a valid recipe, but still a recipe, never a payload.
        let template = json!({"version": 1, "value": {"type": "template", "id": 7}});
        assert_eq!(tag(&template), KIT_TAG);
    }

    #[test]
    fn invalid_groups_are_refused() {
        let mut value = groups_value(GroupsId::Me, &me(&[("g1", "Work")], &[]));
        value["assignments"] = json!({ CHANNEL: "missing" });
        assert!(parse_groups(&value).is_err());
        value["assignments"] = json!({});
        value["groups"] = json!([{"id": "g1", "name": " ", "defaultTemplateId": ""}]);
        assert!(parse_groups(&value).is_err());
        value["groups"] = json!([
            {"id": "g1", "name": "A", "defaultTemplateId": ""},
            {"id": "g1", "name": "B", "defaultTemplateId": ""}
        ]);
        assert_eq!(parse_groups(&value).unwrap_err(), "Duplicate Me group");
    }

    #[test]
    fn capacity_counts_only_when_a_record_exists() {
        let many: Vec<String> = (0..450)
            .map(|i| format!("{:08x}-1111-4111-8111-111111111111", i))
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let value = me(&[], &refs);
        let entry = RawKitEntry {
            record: json!({}),
            event_id: String::new(),
            created_at: 0,
        };
        assert_eq!(
            admit(&value, Some(&entry), COMMUNITY).unwrap_err(),
            ME_CAPACITY
        );
        assert!(admit(&value, None, COMMUNITY).is_ok());
    }

    #[test]
    fn records_round_trip_through_encryption_and_admission() {
        let identity = Identity::ephemeral();
        let value = groups_value(GroupsId::Me, &me(&[("g1", "Work")], &[CHANNEL]));
        let record = record(COMMUNITY, value, false);
        let (content, tags) = prepare(&identity, &record, COMMUNITY).unwrap();
        let unsigned = identity
            .template(buzz_identity::Purpose::AppData, 30078, 1, tags, content)
            .unwrap();
        let event = identity
            .sign(buzz_identity::Purpose::AppData, unsigned)
            .unwrap();
        let verified = crate::event::Verifier::default().verify(event).unwrap();
        let entry = decode(&identity, &verified, COMMUNITY).unwrap();
        assert_eq!(entry.record, record);
        assert_eq!(entry.groups().unwrap().channels, [CHANNEL]);
        assert!(decode(&identity, &verified, "https://other.test").is_err());
        assert!(decode(&Identity::ephemeral(), &verified, COMMUNITY).is_err());
    }
}
