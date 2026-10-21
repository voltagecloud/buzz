//! Direct messages (`docs/port/relay.md` §5e): the confirmation read after
//! an open-DM command (41010), the participant check, and the people
//! picker's profile pages.
//!
//! The 41010 builder and its receipt parser live in [`crate::messages`];
//! hiding (41012) and the 30622 visibility list in [`crate::lifecycle`].
//! Ports `direct-messages.ts`.

use crate::discovery::{ChannelSummary, ChannelType};
use crate::event::{Verified, kinds};
use crate::members::Person;
use crate::socket::{Filter, filter};
use serde_json::json;
use std::collections::BTreeSet;

pub use crate::messages::{open_dm, parse_open_dm_receipt, valid_dm_participants};

/// Longest people-picker query.
pub const MAX_QUERY: usize = 100;

/// Only signed discovery admits the returned channel: a strong read of its
/// metadata and roster. A receipt is never membership.
pub fn confirm_filter(relay_author: &str, channel: &str) -> Filter {
    filter(json!({
        "kinds": [kinds::CHANNEL_METADATA, kinds::CHANNEL_MEMBERS],
        "consistency": "strong",
        "authors": [relay_author],
        "#d": [channel],
        "limit": 2,
    }))
}

/// The viewer's earlier message `id` in `channel`, when the outbox no
/// longer holds it.
pub fn sent_filter(id: &nostr::prelude::EventId, viewer: &str, channel: &str) -> Filter {
    filter(json!({
        "ids": [id.to_hex()],
        "kinds": [kinds::CHAT],
        "authors": [viewer],
        "#h": [channel],
        "limit": 1,
    }))
}

/// The confirmed channel is a live DM of exactly the viewer and `pubkeys`.
pub fn confirmed(channel: Option<&ChannelSummary>, viewer: &str, pubkeys: &[String]) -> bool {
    let Some(channel) = channel else {
        return false;
    };
    let expected: BTreeSet<&str> = pubkeys.iter().map(String::as_str).chain([viewer]).collect();
    let members: BTreeSet<&str> = channel.members.iter().map(String::as_str).collect();
    channel.channel_type == Some(ChannelType::Dm)
        && !channel.archived
        && channel.members.len() == expected.len()
        && members == expected
}

/// One page of the people picker (§5e.6). Browsing shows 15 first, then
/// pages of 30 that overlap the preview; a search pages by 30. `page`
/// starts at 1.
pub fn people_filter(query: &str, page: usize) -> Result<(Filter, usize), String> {
    if query.encode_utf16().count() > MAX_QUERY || page == 0 {
        return Err("Invalid people search.".into());
    }
    let value = query.trim();
    let browsing = value.is_empty();
    let limit = if browsing && page == 1 { 15 } else { 30 };
    let relay_page = if browsing && page > 1 { page - 1 } else { page };
    let mut f = filter(json!({"kinds": [kinds::PROFILE], "limit": limit, "page": relay_page}));
    // Only a lowercase hex key is one (`exactKey`).
    if value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        f.insert("authors".into(), json!([value]));
    } else if !browsing {
        f.insert("search".into(), json!(value));
        f.insert("search_mode".into(), json!("prefix"));
    }
    Ok((f, limit))
}

/// The people on one picker page, without the viewer, in the relay's
/// order. `more` when the page was full.
pub fn people_page(events: &[Verified], viewer: &str, limit: usize) -> (Vec<Person>, bool) {
    let people = crate::members::fold_people(events)
        .into_iter()
        .filter(|person| person.pubkey != viewer)
        .collect();
    (people, events.len() >= limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{CHANNEL, key};

    #[test]
    fn browsing_starts_small_then_pages_by_thirty() {
        let (first, limit) = people_filter("", 1).unwrap();
        assert_eq!((limit, &first["page"]), (15, &json!(1)));
        assert!(!first.contains_key("search"));
        let (second, limit) = people_filter("", 2).unwrap();
        assert_eq!((limit, &second["page"]), (30, &json!(1)));
        let (search, limit) = people_filter("bo", 2).unwrap();
        assert_eq!((limit, &search["page"]), (30, &json!(2)));
        assert_eq!(search["search_mode"], json!("prefix"));
        let (exact, _) = people_filter(&key(3), 1).unwrap();
        assert_eq!(exact["authors"], json!([key(3)]));
        let (upper, _) = people_filter(&key(3).to_uppercase(), 1).unwrap();
        assert!(!upper.contains_key("authors"));
        assert!(people_filter(&"x".repeat(101), 1).is_err());
    }

    #[test]
    fn confirms_exactly_the_requested_participants() {
        let viewer = key(1);
        let mut members = vec![viewer.clone(), key(2)];
        members.sort();
        let dm = ChannelSummary {
            id: CHANNEL.into(),
            channel_type: Some(ChannelType::Dm),
            members,
            ..Default::default()
        };
        assert!(confirmed(Some(&dm), &viewer, &[key(2)]));
        assert!(!confirmed(Some(&dm), &viewer, &[key(2), key(3)]));
        let stream = ChannelSummary {
            channel_type: Some(ChannelType::Stream),
            ..dm.clone()
        };
        assert!(!confirmed(Some(&stream), &viewer, &[key(2)]));
        assert!(!confirmed(None, &viewer, &[key(2)]));
    }

    #[test]
    fn confirmation_reads_metadata_and_roster_strongly() {
        let f = confirm_filter(&key(9), CHANNEL);
        assert_eq!(f["kinds"], json!([39000, 39002]));
        assert_eq!(f["consistency"], json!("strong"));
        assert_eq!(f["#d"], json!([CHANNEL]));
    }
}
