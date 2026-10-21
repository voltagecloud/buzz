//! The Inbox's pure rules: view and sender filters, labels and times
//! (`InboxPage.tsx`, `InboxFilters.tsx`, `dm-label.ts`,
//! `relative-timestamp.ts`, `view-state.ts`).

use crate::ui::keys::format_public_key;
use buzz_relay_client::unread::InboxItem;
use serde_json::{Value, json};

/// The view-state key of the saved filter choices.
pub const FILTERS_KEY: &str = "inbox:filters";

/// The "Type" choices, and Drafts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    All,
    Threads,
    Mentions,
    Dms,
    Drafts,
}

impl View {
    /// The "Type" group; Drafts has its own item.
    pub const TYPES: [View; 4] = [View::All, View::Threads, View::Mentions, View::Dms];

    pub fn label(self) -> &'static str {
        match self {
            View::All => "All",
            View::Threads => "Threads",
            View::Mentions => "Mentions",
            View::Dms => "DMs",
            View::Drafts => "Drafts",
        }
    }
}

/// The "Status" choices: the active Inbox or its archive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Show {
    #[default]
    Inbox,
    Archived,
}

/// The "From" choices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sender {
    #[default]
    Everyone,
    Humans,
    Agents,
}

/// What a sender is known to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Human,
    Agent,
    Unknown,
}

/// The filter trigger's text: "All", "Mentions · Archived · Agents", or
/// just "Archived" for all archived activity.
pub fn summary(view: View, show: Show, sender: Sender) -> String {
    if view == View::Drafts {
        return view.label().to_owned();
    }
    let archived = show == Show::Archived;
    [
        (!(view == View::All && archived)).then(|| view.label()),
        archived.then_some("Archived"),
        match sender {
            Sender::Everyone => None,
            Sender::Humans => Some("People"),
            Sender::Agents => Some("Agents"),
        },
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

/// The choices saved on this device for the account and community
/// (`readFilters`): each missing or unknown field takes its default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    pub show: Show,
    /// Never Drafts, which is not saved.
    pub activity: View,
    pub sender: Sender,
    pub unread_only: bool,
}

impl Filters {
    pub fn read(saved: Option<&str>) -> Self {
        let value: Value = saved
            .and_then(|saved| serde_json::from_str(saved).ok())
            .unwrap_or(Value::Null);
        let field = |key: &str| value.get(key).and_then(Value::as_str);
        Self {
            show: match field("show") {
                Some("archived") => Show::Archived,
                _ => Show::Inbox,
            },
            activity: match field("activity") {
                Some("dms") => View::Dms,
                Some("threads") => View::Threads,
                Some("mentions") => View::Mentions,
                _ => View::All,
            },
            sender: match field("sender") {
                Some("humans") => Sender::Humans,
                Some("agents") => Sender::Agents,
                _ => Sender::Everyone,
            },
            unread_only: field("attention") == Some("unread"),
        }
    }

    pub fn to_json(self) -> String {
        json!({
            "show": match self.show {
                Show::Inbox => "inbox",
                Show::Archived => "archived",
            },
            "activity": match self.activity {
                View::Dms => "dms",
                View::Threads => "threads",
                View::Mentions => "mentions",
                View::All | View::Drafts => "all",
            },
            "sender": match self.sender {
                Sender::Everyone => "everyone",
                Sender::Humans => "humans",
                Sender::Agents => "agents",
            },
            "attention": if self.unread_only { "unread" } else { "all" },
        })
        .to_string()
    }
}

/// The From checkbox `people` (or agents) changed to `checked`; the other
/// kind can never be unchecked as well.
pub fn toggle_sender(current: Sender, people: bool, checked: bool) -> Sender {
    match (people, checked, current) {
        (_, true, _) => Sender::Everyone,
        (true, false, Sender::Everyone) => Sender::Agents,
        (false, false, Sender::Everyone) => Sender::Humans,
        (_, false, current) => current,
    }
}

pub fn matches_activity(item: &InboxItem, view: View, dm: bool) -> bool {
    match view {
        View::All | View::Drafts => true,
        View::Dms => dm,
        View::Mentions => item.mentioned,
        View::Threads => item.thread,
    }
}

pub fn has_unread(item: &InboxItem) -> bool {
    item.unread_count > 0 || item.manual
}

/// One label for DM rows, details and drafts: the first three people,
/// then "+N".
pub fn dm_label(
    participants: Option<&[String]>,
    name: impl Fn(&str, &str) -> String,
    missing: &str,
) -> String {
    let Some(participants) = participants else {
        return missing.to_owned();
    };
    if participants.is_empty() {
        return "Notes to self".into();
    }
    let mut label = participants
        .iter()
        .take(3)
        .map(|key| {
            let fallback = format_public_key(key).unwrap_or_else(|| "Unknown person".into());
            name(key, &fallback)
        })
        .collect::<Vec<_>>()
        .join(", ");
    if participants.len() > 3 {
        label.push_str(&format!(" +{}", participants.len() - 3));
    }
    label
}

/// `relativeTimestamp`: "just now", "5 minutes ago", … then "on Oct 3".
pub fn relative_timestamp(at: u64, now: u64) -> String {
    let age = now.saturating_sub(at);
    let ago =
        |value: u64, unit: &str| format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" });
    match age {
        0..60 => "just now".into(),
        60..3600 => ago(age / 60, "minute"),
        3600..86_400 => ago(age / 3600, "hour"),
        86_400..604_800 => ago(age / 86_400, "day"),
        _ => format!("on {}", super::super::time::short_month_day(at)),
    }
}

/// A composer draft's place (`draftCoordinates`): `draft:<channel>` or
/// `draft:<channel>:thread:<root>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coordinates {
    pub channel_id: String,
    pub thread_root: Option<String>,
}

pub fn draft_coordinates(key: &str) -> Option<Coordinates> {
    let rest = key.strip_prefix("draft:")?;
    let (channel, thread) = match rest.split_once(":thread:") {
        Some((channel, root)) => (channel, Some(root)),
        None => (rest, None),
    };
    let mut chars = channel.chars();
    let first = chars.next()?;
    let valid_channel = first.is_ascii_alphanumeric()
        && channel.len() <= 256
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    let valid_root = thread.is_none_or(|root| {
        root.len() == 64 && root.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    });
    (valid_channel && valid_root).then(|| Coordinates {
        channel_id: channel.to_owned(),
        thread_root: thread.map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn senders_never_both_unchecked() {
        assert_eq!(toggle_sender(Sender::Everyone, true, false), Sender::Agents);
        assert_eq!(toggle_sender(Sender::Agents, false, false), Sender::Agents);
        assert_eq!(toggle_sender(Sender::Agents, true, true), Sender::Everyone);
        assert_eq!(
            summary(View::Mentions, Show::Inbox, Sender::Humans),
            "Mentions · People"
        );
        assert_eq!(
            summary(View::Drafts, Show::Archived, Sender::Agents),
            "Drafts"
        );
        assert_eq!(
            summary(View::All, Show::Archived, Sender::Everyone),
            "Archived"
        );
        assert_eq!(
            summary(View::Dms, Show::Archived, Sender::Agents),
            "DMs · Archived · Agents"
        );
    }

    #[test]
    fn saved_filters_default_field_by_field() {
        let saved = Filters {
            show: Show::Archived,
            activity: View::Mentions,
            sender: Sender::Agents,
            unread_only: true,
        };
        assert_eq!(Filters::read(Some(&saved.to_json())), saved);
        assert_eq!(Filters::read(None), Filters::default());
        // An older combined status, or a bad field, falls back alone.
        let mixed = Filters::read(Some(r#"{"show":"all","activity":"dms","sender":7}"#));
        assert_eq!(
            mixed,
            Filters {
                activity: View::Dms,
                ..Filters::default()
            }
        );
    }

    #[test]
    fn dm_labels_name_three_then_count() {
        let keys: Vec<String> = ["a", "b", "c", "d", "e"].map(String::from).to_vec();
        let name = |key: &str, _: &str| key.to_uppercase();
        assert_eq!(dm_label(Some(&keys), name, "x"), "A, B, C +2");
        assert_eq!(dm_label(Some(&[]), name, "x"), "Notes to self");
        assert_eq!(dm_label(None, name, "Direct message"), "Direct message");
    }

    #[test]
    fn times_read_like_buzz_app() {
        assert_eq!(relative_timestamp(100, 130), "just now");
        assert_eq!(relative_timestamp(0, 60), "1 minute ago");
        assert_eq!(relative_timestamp(0, 7200), "2 hours ago");
        assert_eq!(relative_timestamp(0, 86_400 * 3), "3 days ago");
    }

    #[test]
    fn draft_keys_name_their_place() {
        let root = "a".repeat(64);
        assert_eq!(
            draft_coordinates(&format!("draft:c1:thread:{root}")),
            Some(Coordinates {
                channel_id: "c1".into(),
                thread_root: Some(root),
            })
        );
        assert!(draft_coordinates("draft:-bad").is_none());
        assert!(draft_coordinates("draft:c1:thread:xyz").is_none());
        assert!(draft_coordinates("selected-channel").is_none());
    }
}
