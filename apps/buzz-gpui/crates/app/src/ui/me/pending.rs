//! A new conversation's start, saved on this device as it goes
//! (`sessions/pending-start.ts`): which steps are done and the ids of the
//! operations already queued, so a start interrupted by a failure or a
//! restart resumes without creating a second session.
//!
//! Each draft has a key: `me`, or `me:section:<id>` for a new
//! conversation in a group. buzz-app finds a session's start by scanning
//! local storage; here the keys with a saved start are listed under one
//! view-state entry.

use crate::community::{AppState, Community};
use buzz_relay_client::members::is_key;
use gpui_kit::{App, Entity};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// The view-state entry listing the draft keys with a saved start.
const INDEX: &str = "me:pending-starts";
pub const SAVE_FAILED: &str =
    "Your session start couldn’t be saved on this device. No further changes were sent.";

/// Bumped by every save and clear, so a cached [`draft_key_for`] can
/// tell it is out of date.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The current generation of saved starts.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// The draft key of a new conversation, in a group or not.
pub fn draft_key(section: Option<&str>) -> String {
    match section {
        Some(section) => format!("me:section:{section}"),
        None => "me".into(),
    }
}

/// The group a draft key starts in.
pub fn section_of(key: &str) -> Option<String> {
    key.split_once(":section:")
        .map(|(_, section)| section.to_owned())
}

/// Section defaults applied after creation (`SessionSetup`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_id: Option<String>,
    pub canvas: String,
    pub agents: Vec<String>,
}

/// `PendingStart`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingStart {
    pub id: String,
    pub text: String,
    /// Who the first message notifies, and whom it only names.
    #[serde(default)]
    pub recipients: Vec<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<SessionSetup>,
    #[serde(default)]
    pub setup_done: bool,
    #[serde(default)]
    pub placement_done: bool,
}

impl PendingStart {
    /// `readPending`'s checks: a session UUID, bounded text and hex ids.
    fn valid(&self) -> bool {
        buzz_relay_client::discovery::is_uuid(&self.id)
            && self.text.len() <= 16_000
            && [
                &self.creation_id,
                &self.message_id,
                &self.invitation_id,
                &self.agent,
            ]
            .iter()
            .all(|id| id.as_deref().is_none_or(is_key))
    }
}

fn read_json(community: &Community, key: &str, cx: &App) -> Option<String> {
    cx.global::<AppState>()
        .store
        .view_state(&community.partition(), key)
        .ok()
        .flatten()
}

fn index(community: &Community, cx: &App) -> Vec<String> {
    read_json(community, INDEX, cx)
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// The saved start of a draft, if any and valid.
pub fn read(community: &Entity<Community>, key: &str, cx: &App) -> Option<PendingStart> {
    let community = community.read(cx);
    read_json(community, &format!("{key}:pending"), cx)
        .and_then(|json| serde_json::from_str::<PendingStart>(&json).ok())
        .filter(PendingStart::valid)
}

/// Saves a start; refuses to continue when it cannot be saved.
pub fn save(
    community: &Entity<Community>,
    key: &str,
    start: &PendingStart,
    cx: &App,
) -> Result<(), String> {
    let community = community.read(cx);
    let store = &cx.global::<AppState>().store;
    let partition = community.partition();
    GENERATION.fetch_add(1, Ordering::Relaxed);
    let json = serde_json::to_string(start).map_err(|_| SAVE_FAILED.to_owned())?;
    store
        .set_view_state(&partition, &format!("{key}:pending"), Some(&json))
        .map_err(|_| SAVE_FAILED.to_owned())?;
    let mut keys = index(community, cx);
    if !keys.iter().any(|known| known == key) {
        keys.push(key.to_owned());
        let json = serde_json::to_string(&keys).expect("keys serialize");
        store
            .set_view_state(&partition, INDEX, Some(&json))
            .map_err(|_| SAVE_FAILED.to_owned())?;
    }
    Ok(())
}

/// Forgets a finished or abandoned start.
pub fn clear(community: &Entity<Community>, key: &str, cx: &App) {
    let community = community.read(cx);
    let store = &cx.global::<AppState>().store;
    let partition = community.partition();
    GENERATION.fetch_add(1, Ordering::Relaxed);
    let _ = store.set_view_state(&partition, &format!("{key}:pending"), None);
    let mut keys = index(community, cx);
    keys.retain(|known| known != key);
    let json = serde_json::to_string(&keys).expect("keys serialize");
    let _ = store.set_view_state(&partition, INDEX, Some(&json));
}

/// The draft whose saved start is creating `channel`
/// (`pendingSessionDraft`), including one whose group has since gone.
pub fn draft_key_for(community: &Entity<Community>, channel: &str, cx: &App) -> Option<String> {
    index(community.read(cx), cx)
        .into_iter()
        .find(|key| read(community, key, cx).is_some_and(|start| start.id == channel))
}

/// The draft-scoped Canvas chosen in "Session settings" before the start.
pub fn draft_canvas(community: &Entity<Community>, key: &str, cx: &App) -> Option<String> {
    read_json(community.read(cx), &format!("{key}:settings"), cx)
        .and_then(|json| serde_json::from_str::<Option<String>>(&json).ok())
        .flatten()
}

/// Saves (or with `None`, clears) the draft-scoped Canvas.
pub fn set_draft_canvas(
    community: &Entity<Community>,
    key: &str,
    canvas: Option<&str>,
    cx: &App,
) -> Result<(), String> {
    let json = canvas.map(|canvas| serde_json::to_string(canvas).expect("strings serialize"));
    cx.global::<AppState>()
        .store
        .set_view_state(
            &community.read(cx).partition(),
            &format!("{key}:settings"),
            json.as_deref(),
        )
        .map_err(|_| "Settings couldn’t be saved on this device. Try again.".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn draft_keys_carry_their_group() {
        assert_eq!(draft_key(None), "me");
        assert_eq!(draft_key(Some("g1")), "me:section:g1");
        assert_eq!(section_of("me:section:g1").as_deref(), Some("g1"));
        assert_eq!(section_of("me"), None);
    }

    #[test]
    fn saved_starts_are_validated_like_read_pending() {
        let mut start = PendingStart {
            id: "11111111-1111-4111-8111-111111111111".into(),
            text: "hello".into(),
            ..Default::default()
        };
        assert!(start.valid());
        start.creation_id = Some("nope".into());
        assert!(!start.valid());
        start.creation_id = Some("a".repeat(64));
        assert!(start.valid());
        start.id = "x".into();
        assert!(!start.valid());
        let json = serde_json::to_value(PendingStart {
            setup: Some(SessionSetup {
                section_id: Some("g".into()),
                ..Default::default()
            }),
            placement_done: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(json["setup"]["sectionId"], "g");
        assert_eq!(json["placementDone"], true);
    }
}
