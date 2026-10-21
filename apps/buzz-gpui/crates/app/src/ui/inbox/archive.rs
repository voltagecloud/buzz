//! Inbox archive (`bundled/inbox/archive.ts`, #677): conversations the
//! viewer dismissed from Inbox, saved on this device per community and
//! account. A conversation comes back when there is something new for the
//! viewer: a fresh reply in a participating thread or a new DM message
//! returns it to its ordinary views, and only a fresh explicit mention
//! returns it to Mentions.
//!
//! An archive records a time watermark and the message IDs already seen.
//! Reply reopening is saved without dropping the mention cutoff, so the
//! bounded evidence window can neither hide the reopened conversation nor
//! resurface its older mentions; a fresh mention retires the archive.

use buzz_relay_client::read_state::ReadTarget;
use buzz_relay_client::unread::InboxItem;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// The view-state key of the saved archives.
pub const KEY: &str = "inbox:archives";
const MAX_BYTES: usize = 512 * 1024;
const MAX_ARCHIVES: usize = 1000;
const MAX_MESSAGES: usize = 4096;
/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE: u64 = (1 << 53) - 1;

pub const FULL: &str = "Inbox archive storage is full. Restore some archived conversations first.";
pub const CHANGED: &str = "Inbox archive changed in another window. Try again.";
pub const UNSAVED: &str = "Could not save the Inbox archive on this device. Try again.";
pub const REOPEN_UNSAVED: &str =
    "Could not save the reopened Inbox conversation on this device. Try again.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Archive {
    pub id: String,
    pub channel_id: String,
    pub through: u64,
    pub message_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reopened: Option<bool>,
}

impl Archive {
    fn valid(&self) -> bool {
        self.through <= MAX_SAFE
            && self.message_ids.len() <= MAX_MESSAGES
            && self.message_ids.iter().all(|id| {
                id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            })
    }

    fn reopened(&self) -> bool {
        self.reopened == Some(true)
    }

    /// The saved archive stands for this conversation (`owns`).
    fn owns(&self, item: &InboxItem) -> bool {
        self.channel_id == item.channel_id
            && (self.id == item.id
                || self
                    .message_ids
                    .iter()
                    .any(|id| item.message_ids.contains(id)))
    }
}

/// The saved archives; malformed ones are dropped (`readArchives`).
pub fn read(revision: Option<&str>) -> Vec<Archive> {
    let Some(revision) = revision.filter(|revision| revision.len() <= MAX_BYTES) else {
        return Vec::new();
    };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(revision) else {
        return Vec::new();
    };
    if entries.len() > MAX_ARCHIVES {
        return Vec::new();
    }
    entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value::<Archive>(entry).ok())
        .filter(Archive::valid)
        .collect()
}

/// Archives by `(channel, conversation or message id)` (`archiveIndex`).
#[derive(Clone, Debug, Default)]
pub struct Index {
    archives: Vec<Archive>,
    by_id: HashMap<(String, String), Vec<usize>>,
}

impl Index {
    pub fn new(archives: Vec<Archive>) -> Self {
        let mut by_id: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (index, archive) in archives.iter().enumerate() {
            for id in std::iter::once(&archive.id).chain(&archive.message_ids) {
                by_id
                    .entry((archive.channel_id.clone(), id.clone()))
                    .or_default()
                    .push(index);
            }
        }
        Self { archives, by_id }
    }

    pub fn is_empty(&self) -> bool {
        self.archives.is_empty()
    }

    /// The archives this conversation's evidence names, each once.
    fn matches(&self, item: &InboxItem) -> Vec<usize> {
        let mut seen = HashSet::new();
        std::iter::once(&item.id)
            .chain(&item.message_ids)
            .flat_map(|id| {
                self.by_id
                    .get(&(item.channel_id.clone(), id.clone()))
                    .into_iter()
                    .flatten()
                    .copied()
            })
            .filter(|index| seen.insert(*index))
            .collect()
    }
}

/// DMs are grouped under a channel target, top-level messages and replies
/// alike; both renew ordinary activity.
fn ordinary_activity(item: &InboxItem) -> bool {
    item.thread || matches!(item.target, ReadTarget::Channel { .. })
}

/// A message the archive had not seen, at or after its watermark, that
/// counts for this view.
fn renewed(archive: &Archive, item: &InboxItem, mentions_only: bool) -> bool {
    item.messages.iter().any(|message| {
        (message.mentioned || (!mentions_only && ordinary_activity(item)))
            && !archive.message_ids.contains(&message.id)
            && message.created_at >= archive.through
    })
}

/// The conversation is archived in this view; Mentions asks for a fresh
/// explicit mention to bring it back (`isArchived`).
pub fn is_archived(index: &Index, item: &InboxItem, mentions_only: bool) -> bool {
    if index.is_empty() {
        return false;
    }
    let entries = index.matches(item);
    !entries.is_empty()
        && !entries.iter().any(|&entry| {
            let archive = &index.archives[entry];
            (!mentions_only && ordinary_activity(item) && archive.reopened())
                || renewed(archive, item, mentions_only)
        })
}

/// The archives after archiving (or restoring) a conversation, as JSON to
/// save (`updateArchive`). Archiving records `at`, or the conversation's
/// newest time if later.
pub fn updated(
    revision: Option<&str>,
    item: &InboxItem,
    archived: bool,
    at: u64,
) -> Result<String, &'static str> {
    let mut archives: Vec<Archive> = read(revision)
        .into_iter()
        .filter(|entry| !entry.owns(item))
        .collect();
    if archived {
        archives.push(Archive {
            id: item.id.clone(),
            channel_id: item.channel_id.clone(),
            through: at.max(item.created_at),
            message_ids: item.message_ids.clone(),
            reopened: None,
        });
    }
    let json = serde_json::to_string(&archives).expect("archives serialize");
    if archives.len() > MAX_ARCHIVES || json.len() > MAX_BYTES {
        return Err(FULL);
    }
    Ok(json)
}

/// Saves reply reopening and retires archives a fresh mention renewed,
/// keeping the mention cutoff otherwise; follows a conversation that
/// regrouped under its verified root (`reopenArchives`). `None`: nothing
/// changed.
pub fn reopened(revision: Option<&str>, items: &[InboxItem]) -> Option<String> {
    let archives = read(revision);
    if archives.is_empty() {
        return None;
    }
    let index = Index::new(archives.clone());
    let mut reopen = HashSet::new();
    let mut retired = HashSet::new();
    // The one conversation each archive matched; split evidence (`None`)
    // does not identify a replacement.
    let mut regrouped: HashMap<usize, Option<&InboxItem>> = HashMap::new();
    for item in items {
        for entry in index.matches(item) {
            let archive = &archives[entry];
            if renewed(archive, item, true) {
                retired.insert(entry);
            } else if !archive.reopened() && renewed(archive, item, false) {
                reopen.insert(entry);
            }
            regrouped
                .entry(entry)
                .and_modify(|seen| *seen = None)
                .or_insert(Some(item));
        }
    }
    let mut changed = !reopen.is_empty() || !retired.is_empty();
    let retained: Vec<Archive> = archives
        .iter()
        .enumerate()
        .filter(|(entry, _)| !retired.contains(entry))
        .map(|(entry, archive)| {
            let mut archive = archive.clone();
            if reopen.contains(&entry) {
                archive.reopened = Some(true);
            }
            // Never downgrade a verified root to an unresolved reply.
            if let Some(Some(item)) = regrouped.get(&entry)
                && item.root_id.is_some()
                && archive.id != item.id
            {
                archive.id = item.id.clone();
                changed = true;
            }
            archive
        })
        .collect();
    changed.then(|| serde_json::to_string(&retained).expect("archives serialize"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::unread::InboxMessage;
    use core::prelude::v1::test;

    fn id(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn item() -> InboxItem {
        let mention = id('a');
        InboxItem {
            id: format!("room:{mention}"),
            channel_id: "room".into(),
            target: ReadTarget::message("room", &mention),
            message_id: mention.clone(),
            latest_message_id: mention.clone(),
            message_ids: vec![mention.clone()],
            root_id: None,
            author_id: "f".repeat(64),
            workflow_owner_id: None,
            preview: "Please review".into(),
            created_at: 20,
            mentioned: true,
            messages: vec![InboxMessage {
                id: mention,
                created_at: 20,
                mentioned: true,
            }],
            thread: false,
            unread_count: 1,
            manual: false,
            read_through: Vec::new(),
        }
    }

    fn with(item: &InboxItem, message: char, at: u64, mentioned: bool) -> InboxItem {
        let mut next = item.clone();
        next.latest_message_id = id(message);
        next.message_ids.push(id(message));
        next.created_at = at;
        next.messages.push(InboxMessage {
            id: id(message),
            created_at: at,
            mentioned,
        });
        next
    }

    fn archived(revision: &Option<String>, item: &InboxItem, mentions: bool) -> bool {
        is_archived(&Index::new(read(revision.as_deref())), item, mentions)
    }

    #[test]
    fn older_mentions_stay_archived_and_a_same_second_tag_reopens() {
        let item = item();
        let saved = Some(updated(None, &item, true, 30).unwrap());
        assert!(archived(&saved, &item, false));
        assert!(archived(&saved, &with(&item, 'b', 40, false), false));
        assert!(archived(&saved, &with(&item, 'c', 29, true), false));
        let fresh = with(&item, 'd', 30, true);
        assert!(!archived(&saved, &fresh, false));
        let next = reopened(saved.as_deref(), &[fresh]);
        assert_eq!(read(next.as_deref()), []);
    }

    #[test]
    fn dm_arrivals_reopen_durably_without_resurfacing_old_mentions() {
        for thread in [false, true] {
            let mut dm = item();
            dm.target = ReadTarget::channel("room");
            dm.thread = thread;
            let saved = Some(updated(None, &dm, true, 30).unwrap());
            let delayed = with(&dm, 'd', 29, false);
            assert!(archived(&saved, &delayed, false));
            assert_eq!(reopened(saved.as_deref(), &[delayed]), None);
            let fresh = with(&dm, 'b', 30, false);
            assert!(!archived(&saved, &fresh, false));
            assert!(archived(&saved, &fresh, true));
            let next = reopened(saved.as_deref(), std::slice::from_ref(&fresh));
            let mut expected = read(saved.as_deref());
            expected[0].reopened = Some(true);
            assert_eq!(read(next.as_deref()), expected);
            // Reopening survives evidence eviction; the old tag stays.
            assert!(!archived(&next, &dm, false));
            assert!(archived(&next, &dm, true));
            let again = Some(updated(next.as_deref(), &fresh, true, 31).unwrap());
            assert!(archived(&again, &fresh, false));
            let mention = with(&fresh, 'c', 31, true);
            assert!(!archived(&again, &mention, true));
            assert_eq!(read(reopened(again.as_deref(), &[mention]).as_deref()), []);
        }
    }

    #[test]
    fn participating_replies_reopen_and_a_fresh_mention_retires() {
        let mut thread = item();
        thread.thread = true;
        let saved = Some(updated(None, &thread, true, 30).unwrap());
        let mut observed = thread.clone();
        observed.messages[0].created_at = 40;
        let delayed = with(&thread, 'd', 29, false);
        assert!(archived(&saved, &observed, false));
        assert!(archived(&saved, &delayed, false));
        assert_eq!(reopened(saved.as_deref(), &[observed, delayed]), None);
        let reply = with(&thread, 'b', 30, false);
        assert!(!archived(&saved, &reply, false));
        assert!(archived(&saved, &reply, true));
        let next = reopened(saved.as_deref(), std::slice::from_ref(&reply));
        assert_eq!(
            read(next.as_deref()),
            [Archive {
                id: thread.id.clone(),
                channel_id: "room".into(),
                through: 30,
                message_ids: thread.message_ids.clone(),
                reopened: Some(true),
            }]
        );
        assert!(!archived(&next, &thread, false));
        assert!(archived(&next, &thread, true));
        assert_eq!(
            reopened(next.as_deref(), &[reply.clone(), thread.clone()]),
            None
        );
        let mention = with(&reply, 'c', 31, true);
        let retired = reopened(next.as_deref(), &[mention]);
        assert_eq!(read(retired.as_deref()), []);
    }

    #[test]
    fn rearchiving_resets_reopening_and_restore_clears() {
        let mut thread = item();
        thread.thread = true;
        let saved = updated(None, &thread, true, 30).unwrap();
        let reply = with(&thread, 'b', 30, false);
        let next = reopened(Some(&saved), std::slice::from_ref(&reply)).unwrap();
        let again = Some(updated(Some(&next), &reply, true, 31).unwrap());
        assert!(archived(&again, &reply, false));
        assert_eq!(read(again.as_deref())[0].reopened, None);
        assert_eq!(
            read(Some(&updated(again.as_deref(), &reply, false, 32).unwrap())),
            []
        );
    }

    #[test]
    fn regrouped_evidence_keeps_and_moves_the_archive() {
        let item = item();
        let saved = Some(updated(None, &item, true, 30).unwrap());
        let mut regrouped = item.clone();
        regrouped.id = "room:root".into();
        assert!(archived(&saved, &regrouped, false));
        let mut elsewhere = regrouped.clone();
        elsewhere.channel_id = "another-room".into();
        assert!(!archived(&saved, &elsewhere, false));
        let restored = Some(updated(saved.as_deref(), &regrouped, false, 30).unwrap());
        assert!(!archived(&restored, &regrouped, false));

        // A verified root takes over the archive's coordinate.
        let root = id('e');
        let mut rooted = item.clone();
        rooted.id = format!("room:{root}");
        rooted.root_id = Some(root);
        rooted.message_ids.push(id('b'));
        let moved = reopened(saved.as_deref(), &[rooted.clone()]);
        assert_eq!(read(moved.as_deref())[0].id, rooted.id);
        // Split evidence names no replacement.
        let mut other = rooted.clone();
        other.id = "room:other".into();
        assert_eq!(reopened(saved.as_deref(), &[rooted, other]), None);
    }

    #[test]
    fn malformed_records_are_ignored() {
        assert_eq!(read(Some("not json")), []);
        assert_eq!(read(Some("{}")), []);
        let good = updated(None, &item(), true, 30).unwrap();
        let mixed = format!(
            r#"[{{"id":"x","channelId":"c","through":-1,"messageIds":[]}},{{"id":"y","channelId":"c","through":1,"messageIds":["nothex"]}},{}]"#,
            &good[1..good.len() - 1]
        );
        assert_eq!(read(Some(&mixed)).len(), 1);
    }
}
