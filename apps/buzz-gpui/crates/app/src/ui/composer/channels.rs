//! `#` completion (`ChannelCompletion.tsx`): joined channels first, then
//! open channels the viewer has not joined, found by a relay search, with
//! a few rows kept for them and the status line under the list.

use buzz_relay_client::{ChannelSummary, ChannelType, PublicChannels, RosterStatus};
use std::collections::{HashMap, HashSet};

/// Rows shown at most.
pub const LIMIT: usize = 20;
/// Rows kept for open channels when joined matches would fill the list.
const OPEN_SLOTS: usize = 5;

/// A channel a reference may name: not a DM, session, archived, hidden or
/// unnamed channel, and no line breaks in the name.
fn referable(channel: &ChannelSummary) -> bool {
    !channel.name.trim().is_empty()
        && !channel.name.contains(['\r', '\n'])
        && !channel.archived
        && !channel.hidden
        && !matches!(
            channel.channel_type,
            Some(ChannelType::Dm | ChannelType::Session)
        )
}

/// A joined channel, including one restored from cache while the relay
/// reconfirms it: a reference names a channel, it grants nothing.
pub fn joined(channel: &ChannelSummary, viewer: &str) -> bool {
    referable(channel)
        && (!channel.read_only || channel.cached)
        && channel.members.iter().any(|member| member == viewer)
}

/// An open channel the viewer can preview but has not joined.
pub fn open(channel: &ChannelSummary) -> bool {
    channel.read_only && !channel.private && referable(channel)
}

/// The open-channel lookup for the current query.
pub enum Search<'a> {
    /// Nothing to search, or the list is not ready.
    Idle,
    /// Running; the previous answer's rows that still match stay.
    Loading(&'a [ChannelSummary]),
    Done(&'a PublicChannels),
    Failed,
}

/// What retrying would refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    Channels,
    Search,
    /// The `@` directory search, or the channel's membership.
    People,
    /// The community's custom emoji catalog.
    Emoji,
    /// The saved teams (the channel kit).
    Teams,
    /// A plugin's completion, by key.
    Plugin(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub channel: ChannelSummary,
    pub member: bool,
    /// "Private channel · Not joined · <id when the name repeats>".
    pub detail: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Completion {
    pub items: Vec<Item>,
    pub status: Option<&'static str>,
    pub retry: Option<Retry>,
}

/// The completion for `#query`. `rank` is the composer's name match tier
/// (exact, prefix, word prefix, substring); a query with a space matches
/// only from the start of a name, so prose after `#name` closes the list.
pub fn complete(
    channels: &[ChannelSummary],
    viewer: &str,
    roster: &RosterStatus,
    query: &str,
    search: Search<'_>,
    rank: impl Fn(&str, &str) -> Option<u8>,
) -> Completion {
    let spaced = query.contains(char::is_whitespace);
    let mut names: HashMap<String, usize> = HashMap::new();
    let mut ranked = |channel: &ChannelSummary| {
        *names.entry(channel.name.to_lowercase()).or_default() += 1;
        rank(&channel.name, query).filter(|tier| !spaced || *tier <= 1)
    };
    let order = |a: &(u8, &ChannelSummary), b: &(u8, &ChannelSummary)| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
            .then_with(|| a.1.id.cmp(&b.1.id))
    };
    let mut mine: Vec<(u8, &ChannelSummary)> = channels
        .iter()
        .filter(|channel| joined(channel, viewer))
        .filter_map(|channel| ranked(channel).map(|tier| (tier, channel)))
        .collect();
    mine.sort_by(order);
    let ids: HashSet<&str> = mine.iter().map(|(_, c)| c.id.as_str()).collect();
    // A channel joined since the lookup leaves the open rows.
    let listed: HashSet<&str> = channels
        .iter()
        .filter(|c| !c.read_only)
        .map(|c| c.id.as_str())
        .collect();
    let found: &[ChannelSummary] = match &search {
        Search::Loading(kept) => kept,
        Search::Done(result) => &result.channels,
        Search::Idle | Search::Failed => &[],
    };
    let mut others: Vec<(u8, &ChannelSummary)> = found
        .iter()
        .filter(|c| !ids.contains(c.id.as_str()) && !listed.contains(c.id.as_str()) && open(c))
        .filter_map(|channel| ranked(channel).map(|tier| (tier, channel)))
        .collect();
    others.sort_by(order);
    // Open rows follow joined ones, so a late search result never moves the
    // row Enter would pick.
    let shown_mine = mine.len().min(LIMIT - others.len().min(OPEN_SLOTS));
    let shown_others = others.len().min(LIMIT - shown_mine);
    let item = |channel: &ChannelSummary, member: bool| {
        let duplicate = names
            .get(&channel.name.to_lowercase())
            .is_some_and(|count| *count > 1);
        let parts: Vec<&str> = [
            channel.private.then_some("Private channel"),
            (!member).then_some("Not joined"),
            duplicate.then_some(channel.id.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect();
        Item {
            channel: channel.clone(),
            member,
            detail: (!parts.is_empty()).then(|| parts.join(" · ")),
        }
    };
    let items: Vec<Item> = mine[..shown_mine]
        .iter()
        .map(|(_, c)| item(c, true))
        .chain(others[..shown_others].iter().map(|(_, c)| item(c, false)))
        .collect();
    let list_error = matches!(roster, RosterStatus::Error(_));
    let settling = matches!(roster, RosterStatus::Loading);
    let searching = matches!(search, Search::Loading(_));
    let failed = matches!(search, Search::Failed);
    let partial = matches!(search, Search::Done(result) if result.partial);
    let status = if list_error {
        Some("Could not refresh channels.")
    } else if items.is_empty() && settling {
        Some("Loading channels…")
    } else if searching {
        Some("Searching open channels…")
    } else if failed {
        Some("Could not search open channels.")
    } else if mine.len() + others.len() > LIMIT {
        Some("Keep typing to narrow the list.")
    } else if partial && !items.is_empty() && others.is_empty() {
        // Prose stays silent: the note joins rows already shown.
        Some("Only the newest open channels were searched.")
    } else {
        None
    };
    let retry = if list_error {
        Some(Retry::Channels)
    } else if failed {
        Some(Retry::Search)
    } else {
        None
    };
    Completion {
        items,
        status,
        retry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_relay_client::Visibility;

    const ME: &str = "me";

    fn channel(id: &str, name: &str) -> ChannelSummary {
        ChannelSummary {
            id: id.into(),
            name: name.into(),
            visibility: Some(Visibility::Public),
            channel_type: Some(ChannelType::Stream),
            members: vec![ME.into()],
            ..ChannelSummary::default()
        }
    }

    fn preview(id: &str, name: &str) -> ChannelSummary {
        ChannelSummary {
            read_only: true,
            members: Vec::new(),
            ..channel(id, name)
        }
    }

    fn rank(name: &str, query: &str) -> Option<u8> {
        let (name, query) = (name.to_lowercase(), query.to_lowercase());
        if name == query {
            Some(0)
        } else if name.starts_with(&query) {
            Some(1)
        } else if name.contains(&query) {
            Some(3)
        } else {
            None
        }
    }

    fn names(completion: &Completion) -> Vec<&str> {
        completion
            .items
            .iter()
            .map(|item| item.channel.name.as_str())
            .collect()
    }

    #[test]
    fn joined_rows_need_membership_and_a_one_line_name() {
        let mut cached = channel("c2", "general-cache");
        cached.read_only = true;
        cached.cached = true;
        let mut outsider = channel("c3", "general-other");
        outsider.members.clear();
        let lines = channel("c4", "general\nbreak");
        let channels = [channel("c1", "general"), cached, outsider, lines];
        let done = complete(
            &channels,
            ME,
            &RosterStatus::Ready,
            "gen",
            Search::Idle,
            rank,
        );
        assert_eq!(names(&done), ["general", "general-cache"]);
        assert_eq!(done.status, None);
    }

    #[test]
    fn ties_order_by_name_then_id_and_name_duplicates() {
        let channels = [channel("b", "ops"), channel("a", "Ops")];
        let done = complete(
            &channels,
            ME,
            &RosterStatus::Ready,
            "op",
            Search::Idle,
            rank,
        );
        let ids: Vec<&str> = done.items.iter().map(|i| i.channel.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(done.items[0].detail.as_deref(), Some("a"));
    }

    #[test]
    fn open_channels_follow_with_reserved_rows() {
        let channels: Vec<ChannelSummary> = (0..25)
            .map(|n| channel(&format!("j{n:02}"), &format!("team-{n:02}")))
            .collect();
        let found = PublicChannels {
            channels: (0..3)
                .map(|n| preview(&format!("o{n}"), &format!("team-open-{n}")))
                .collect(),
            partial: false,
        };
        let done = complete(
            &channels,
            ME,
            &RosterStatus::Ready,
            "team",
            Search::Done(&found),
            rank,
        );
        assert_eq!(done.items.len(), LIMIT);
        assert_eq!(done.items.iter().filter(|i| i.member).count(), LIMIT - 3);
        let last = done.items.last().unwrap();
        assert!(!last.member);
        assert_eq!(last.detail.as_deref(), Some("Not joined"));
        assert_eq!(done.status, Some("Keep typing to narrow the list."));
    }

    #[test]
    fn a_search_result_joined_since_leaves_the_open_rows() {
        let channels = [channel("o1", "design")];
        let found = PublicChannels {
            channels: vec![preview("o1", "design"), preview("o2", "design-crit")],
            partial: true,
        };
        let done = complete(
            &channels,
            ME,
            &RosterStatus::Ready,
            "design",
            Search::Done(&found),
            rank,
        );
        assert_eq!(names(&done), ["design", "design-crit"]);
        assert!(done.items[0].member && !done.items[1].member);
        assert_eq!(done.status, None, "open rows shown: no partial note");
    }

    #[test]
    fn status_lines_and_retries() {
        let channels = [channel("c1", "general")];
        let loading = complete(&[], ME, &RosterStatus::Loading, "x", Search::Idle, rank);
        assert_eq!(loading.status, Some("Loading channels…"));
        let error = complete(
            &channels,
            ME,
            &RosterStatus::Error("down".into()),
            "gen",
            Search::Idle,
            rank,
        );
        assert_eq!(error.status, Some("Could not refresh channels."));
        assert_eq!(error.retry, Some(Retry::Channels));
        let kept = [preview("o1", "general-open")];
        let searching = complete(
            &channels,
            ME,
            &RosterStatus::Ready,
            "gen",
            Search::Loading(&kept),
            rank,
        );
        assert_eq!(searching.status, Some("Searching open channels…"));
        assert_eq!(
            searching.items.len(),
            2,
            "the last answer stays while searching"
        );
        let failed = complete(
            &channels,
            ME,
            &RosterStatus::Ready,
            "gen",
            Search::Failed,
            rank,
        );
        assert_eq!(failed.status, Some("Could not search open channels."));
        assert_eq!(failed.retry, Some(Retry::Search));
        let partial = PublicChannels {
            channels: Vec::new(),
            partial: true,
        };
        let done = complete(
            &channels,
            ME,
            &RosterStatus::Ready,
            "gen",
            Search::Done(&partial),
            rank,
        );
        assert_eq!(
            done.status,
            Some("Only the newest open channels were searched.")
        );
    }

    #[test]
    fn spaced_queries_match_only_name_starts() {
        let channels = [
            channel("c1", "release notes"),
            channel("c2", "old release notes"),
        ];
        let done = complete(
            &channels,
            ME,
            &RosterStatus::Ready,
            "release n",
            Search::Idle,
            rank,
        );
        assert_eq!(names(&done), ["release notes"]);
    }
}
