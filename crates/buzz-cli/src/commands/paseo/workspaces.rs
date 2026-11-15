//! Shared Paseo workspace contract. The bridge and `workspace list --owner`
//! must use `load`, including its validation, ordering, caps, and directories.
use std::collections::{BTreeMap, HashSet};

use buzz_core::kind::KIND_PROJECT;
use nostr::{Event, PublicKey};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{client::BuzzClient, error::CliError};

/// Maximum number of active workspaces per owner.
pub(crate) const MAX_WORKSPACES: usize = 32;
/// Maximum distinct GitHub sources across active workspaces.
pub(crate) const MAX_SOURCES: usize = 64;
/// Exhaustive read bound; exceeding it fails the lookup, never truncates it.
const READ_BOUND: u32 = 10_000;

/// Validated owner-signed workspace, serialized by `workspace list`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct Workspace {
    pub owner: PublicKey,
    pub slug: String,
    pub name: String,
    pub channel: Uuid,
    pub sources: Vec<String>,
    pub directory: String,
    pub created_at: u64,
}

/// A rejected record, suitable for stderr without polluting list JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Skipped {
    pub slug: String,
    pub reason: String,
}

/// Complete selection; callers must not reimplement filtering or ordering.
#[derive(Debug, Default)]
pub(crate) struct Selection {
    pub workspaces: Vec<Workspace>,
    pub skipped: Vec<Skipped>,
}

/// Stable directory identity, without filesystem access or symlink resolution.
pub(crate) fn directory_name(slug: &str) -> String {
    let bytes = slug.as_bytes();
    if !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(b))
    {
        slug.to_owned()
    } else {
        format!("_{}", &hex::encode(Sha256::digest(bytes))[..16])
    }
}

/// Require a canonical HTTPS GitHub repository URL, with no URL decorations.
pub(crate) fn validate_source(source: &str) -> Result<(), String> {
    let valid_part = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    let path = source
        .strip_prefix("https://github.com/")
        .ok_or("source must be https://github.com/<owner>/<repo>")?;
    let parts: Vec<_> = path.split('/').collect();
    if source.len() > 256 || parts.len() != 2 || !parts.iter().all(|s| valid_part(s)) {
        return Err("source must be at most 256 bytes and name one GitHub repository".into());
    }
    Ok(())
}

fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event
        .tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(name))
        .and_then(|t| t.as_slice().get(1).map(String::as_str))
}

/// Exact marker guard, also used before mutation of an existing project.
pub(crate) fn marked(event: &Event) -> bool {
    let tags: Vec<_> = event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("buzz-paseo"))
        .collect();
    tags.len() == 1 && tags[0].as_slice() == ["buzz-paseo", "workspace"]
}

/// Channel policy for both readers and writers. Metadata comes from the relay's
/// authenticated, strong kind:39000 query, never from project tags.
pub(crate) fn valid_channel(channel: Uuid, metadata: &[Value]) -> bool {
    let matches: Vec<_> = metadata
        .iter()
        .filter(|e| {
            e["kind"] == 39000
                && e["tags"].as_array().is_some_and(|tags| {
                    tags.iter().any(|t| {
                        t[0] == "d"
                            && t[1]
                                .as_str()
                                .is_some_and(|s| Uuid::parse_str(s).ok() == Some(channel))
                    })
                })
        })
        .collect();
    if matches.len() != 1 {
        return false;
    }
    let Some(tags) = matches[0]["tags"].as_array() else {
        return false;
    };
    let types: Vec<_> = tags.iter().filter(|t| t[0] == "t").collect();
    types.len() == 1
        && matches!(types[0][1].as_str(), Some("stream" | "forum"))
        && !tags.iter().any(|t| t[0] == "archived" && t[1] != "false")
}

fn parse(event: &Event, metadata: &[Value]) -> Result<Workspace, String> {
    buzz_sdk::validate_project_envelope(
        &event.tags.iter().cloned().collect::<Vec<_>>(),
        &event.content,
    )
    .map_err(|e| e.to_string())?;
    if !marked(event) {
        return Err("requires exactly one buzz-paseo workspace marker".into());
    }
    if event
        .tags
        .iter()
        .any(|t| t.as_slice().first().map(String::as_str) == Some("auth"))
    {
        return Err("NIP-OA agent records are not workspaces".into());
    }
    let slug = tag_value(event, "d").ok_or("missing slug")?;
    let channel = Uuid::parse_str(tag_value(event, "buzz-channel").ok_or("missing channel")?)
        .map_err(|_| "invalid channel UUID")?;
    if !valid_channel(channel, metadata) {
        return Err("channel must be an unarchived stream or forum".into());
    }
    let mut sources = Vec::new();
    for tag in event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("buzz-source"))
    {
        let [_, source] = tag.as_slice() else {
            return Err("invalid source tag".into());
        };
        validate_source(source)?;
        sources.push(source.clone());
    }
    if sources.len() > 16 {
        return Err("workspace exceeds 16 sources".into());
    }
    sources.sort();
    sources.dedup();
    Ok(Workspace {
        owner: event.pubkey,
        slug: slug.into(),
        name: tag_value(event, "name").unwrap_or(slug).into(),
        channel,
        sources,
        directory: directory_name(slug),
        created_at: event.created_at.as_secs(),
    })
}

/// Validate signed heads and select a deterministic bounded set. Newest heads
/// are chosen before validation, so an invalid replacement cannot revive an
/// older destination. Unmarked projects do not consume workspace slots.
pub(crate) fn select(owner: PublicKey, events: &[Event], metadata: &[Value]) -> Selection {
    select_with_directory(owner, events, metadata, directory_name)
}

fn select_with_directory(
    owner: PublicKey,
    events: &[Event],
    metadata: &[Value],
    directory: impl Fn(&str) -> String,
) -> Selection {
    let mut result = Selection::default();
    let mut heads: BTreeMap<&str, &Event> = BTreeMap::new();
    for event in events {
        let slug = tag_value(event, "d").unwrap_or("<missing>");
        if event.pubkey != owner
            || u32::from(event.kind.as_u16()) != KIND_PROJECT
            || event.verify().is_err()
        {
            result.skipped.push(Skipped {
                slug: slug.into(),
                reason: "invalid owner, kind, or signature".into(),
            });
            continue;
        }
        if heads.get(slug).is_none_or(|head| {
            (event.created_at, std::cmp::Reverse(event.id))
                > (head.created_at, std::cmp::Reverse(head.id))
        }) {
            heads.insert(slug, event);
        }
    }
    let mut candidates = Vec::new();
    for (slug, head) in heads {
        if !head
            .tags
            .iter()
            .any(|t| t.as_slice().first().map(String::as_str) == Some("buzz-paseo"))
        {
            continue;
        }
        match parse(head, metadata) {
            Ok(mut workspace) => {
                workspace.directory = directory(slug);
                candidates.push(workspace);
            }
            Err(reason) => result.skipped.push(Skipped {
                slug: slug.into(),
                reason,
            }),
        }
    }
    candidates.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.slug.cmp(&b.slug)));
    let mut counts = BTreeMap::new();
    for w in &candidates {
        *counts.entry(w.directory.clone()).or_insert(0) += 1;
    }
    let mut sources = HashSet::new();
    for workspace in candidates {
        let mut next_sources = sources.clone();
        next_sources.extend(workspace.sources.iter().cloned());
        let reason = if counts[&workspace.directory] > 1 {
            Some("directory collision")
        } else if result.workspaces.len() >= MAX_WORKSPACES {
            Some("32 workspace cap")
        } else if next_sources.len() > MAX_SOURCES {
            Some("64 distinct source cap")
        } else {
            None
        };
        if let Some(reason) = reason {
            result.skipped.push(Skipped {
                slug: workspace.slug,
                reason: reason.into(),
            });
        } else {
            sources = next_sources;
            result.workspaces.push(workspace);
        }
    }
    result
}

/// Strong, exhaustive read shared by bridge routing and daemon CLI output.
/// A failed read is an error, never an empty or partially validated selection.
pub(crate) async fn load(client: &BuzzClient, owner: PublicKey) -> Result<Selection, CliError> {
    let values = client
        .query_all_bounded(
            json!({"consistency":"strong", "kinds":[KIND_PROJECT], "authors":[owner]}),
            READ_BOUND,
        )
        .await?;
    let events: Vec<Event> = values
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(|e| CliError::Other(format!("invalid workspace response: {e}")))?;
    let ids: HashSet<_> = events
        .iter()
        .filter_map(|e| tag_value(e, "buzz-channel").and_then(|value| Uuid::parse_str(value).ok()))
        .map(|id| id.to_string())
        .collect();
    let mut metadata = Vec::new();
    for chunk in ids.into_iter().collect::<Vec<_>>().chunks(256) {
        metadata.extend(
            client
                .query_all_bounded(
                    json!({"consistency":"strong", "kinds":[39000], "#d":chunk}),
                    READ_BOUND,
                )
                .await?,
        );
    }
    Ok(select(owner, &events, &metadata))
}

#[cfg(test)]
#[path = "workspaces_tests.rs"]
mod tests;
