//! Channel members (`docs/port/relay.md` §5g): adding people (9000 through
//! the outbox), role changes and removal (9000/9001 published directly),
//! the mention preflight, and member search over kind 0.
//!
//! Pure. Roles and authority come from [`ChannelState`], one strong read of
//! the channel's 39000/39001/39002. Ports `members.ts`,
//! `administration-protocol.ts` and `channel-members/search.ts`.

use crate::discovery::{ChannelSummary, ChannelType, Visibility};
use crate::event::{Verified, is_hex64, kinds};
use crate::lifecycle::{ChannelState, LifecycleError, MemberRole};
use crate::messages::{Draft, EventTemplate};
use crate::profiles::Profile;
use crate::socket::{Filter, filter};
use nostr::nips::nip19::FromBech32;
use nostr::prelude::PublicKey;
use serde_json::json;
use std::cmp::Ordering;
use std::collections::HashMap;

/// Member search page size (`MEMBER_SEARCH_PAGE_SIZE`).
pub const SEARCH_PAGE: usize = 30;
const MAX_QUERY: usize = 256;

// ---------------------------------------------------------------------------
// Adding members (§5g.2)

/// `canAddMembers`: a live stream or forum the viewer belongs to, or a
/// public preview anyone may add themselves and others to.
pub fn can_add_members(channel: &ChannelSummary, member: bool) -> bool {
    !channel.archived
        && matches!(
            channel.channel_type,
            Some(ChannelType::Stream | ChannelType::Forum)
        )
        && (member || (channel.read_only && channel.visibility == Some(Visibility::Public)))
}

/// An ordinary invitation, delivered by the outbox. Agents join as `bot`;
/// anyone else keeps their role or becomes a member.
pub fn add(channel: &str, pubkey: &str, agent: bool) -> Result<Draft, LifecycleError> {
    if !crate::discovery::is_uuid(channel) || !is_hex64(pubkey) {
        return Err(LifecycleError("Invalid member addition".into()));
    }
    let mut tags = vec![
        vec!["h".to_owned(), channel.to_owned()],
        vec!["p".to_owned(), pubkey.to_owned()],
    ];
    if agent {
        tags.push(vec!["role".to_owned(), "bot".to_owned()]);
    }
    Ok(Draft {
        kind: kinds::PUT_USER,
        content: String::new(),
        tags,
    })
}

/// The outbox recovery key that keeps one addition per channel and person.
pub fn addition_key(channel: &str, pubkey: &str) -> String {
    format!("member-add:{channel}:{pubkey}")
}

/// A lowercase hex public key.
pub fn is_key(value: &str) -> bool {
    is_hex64(value)
}

/// `canShareSession`: a session share adds only to a private work session
/// the viewer belongs to, never through ordinary Members.
pub fn can_share_session(channel: &ChannelSummary, viewer: &str) -> bool {
    channel.channel_type == Some(ChannelType::Session)
        && channel.private
        && !channel.read_only
        && !channel.cached
        && !channel.archived
        && channel.members.iter().any(|member| member == viewer)
}

/// Whether an outbox operation is an ordinary addition of `pubkey` to
/// `channel` (a 9000 with no role other than `bot`).
pub fn is_addition(op: &crate::outbox::Operation, channel: &str, pubkey: &str) -> bool {
    let tags: Vec<&[String]> = op.event.tags.iter().map(|tag| tag.as_slice()).collect();
    let has = |name: &str, value: &str| {
        tags.iter().any(|tag| {
            tag.first().map(String::as_str) == Some(name)
                && tag.get(1).map(String::as_str) == Some(value)
        })
    };
    op.kind() == kinds::PUT_USER
        && has("h", channel)
        && has("p", pubkey)
        && !tags.iter().any(|tag| {
            tag.first().map(String::as_str) == Some("role")
                && tag.get(1).map(String::as_str) != Some("bot")
        })
}

// ---------------------------------------------------------------------------
// Administration (§5g.3)

/// What an administrator may set (`editableMemberRoles`). Owner and bot
/// are never granted here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Admin,
    Member,
    Guest,
    Remove,
}

impl Change {
    fn role(self) -> Option<MemberRole> {
        match self {
            Self::Admin => Some(MemberRole::Admin),
            Self::Member => Some(MemberRole::Member),
            Self::Guest => Some(MemberRole::Guest),
            Self::Remove => None,
        }
    }
}

/// One administrative change, valid only while the member still has
/// `expected`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberChange {
    pub pubkey: String,
    pub expected: MemberRole,
    pub change: Change,
}

const CHANGED: &str =
    "Membership or permissions changed. Refresh members and roles before trying again.";

/// `authorizeMemberChange`, against a fresh state. `owner` is the target
/// agent's verified owner, which permits removal only.
pub fn authorize(
    state: &ChannelState,
    viewer: &str,
    change: &MemberChange,
    owner: Option<&str>,
) -> Result<(), LifecycleError> {
    let allowed = match change.change {
        Change::Remove => state.can_remove_member(viewer, &change.pubkey, owner),
        _ => state.can_manage_member(viewer, &change.pubkey),
    };
    if !allowed
        || state.role(&change.pubkey) != Some(change.expected)
        || change.change.role() == Some(change.expected)
    {
        return Err(LifecycleError(CHANGED.into()));
    }
    Ok(())
}

/// The exact command: `[h, p, role]` for 9000 or `[h, p]` for 9001.
pub fn administration(
    channel: &str,
    change: &MemberChange,
    viewer: &str,
    now: u64,
) -> Result<EventTemplate, LifecycleError> {
    if !crate::discovery::is_uuid(channel) || !is_hex64(&change.pubkey) || change.pubkey == viewer {
        return Err(LifecycleError(
            "Invalid member administration command".into(),
        ));
    }
    let mut tags = vec![
        vec!["h".to_owned(), channel.to_owned()],
        vec!["p".to_owned(), change.pubkey.clone()],
    ];
    let kind = match change.change.role() {
        Some(role) => {
            tags.push(vec!["role".to_owned(), role.as_str().to_owned()]);
            kinds::PUT_USER
        }
        None => kinds::REMOVE_USER,
    };
    Ok(EventTemplate {
        kind,
        created_at: now,
        tags,
        content: String::new(),
    })
}

/// The strong read of an agent's kind 0 whose NIP-OA attestation lets its
/// owner remove it.
pub fn owner_filter(agent: &str) -> Filter {
    filter(json!({
        "kinds": [kinds::PROFILE],
        "authors": [agent],
        "limit": 5,
        "consistency": "strong",
    }))
}

/// `memberChangeConfirmed`: removed members are gone, others hold the role.
pub fn change_confirmed(state: &ChannelState, change: &MemberChange) -> bool {
    match change.change.role() {
        None => state.role(&change.pubkey).is_none(),
        Some(role) => state.role(&change.pubkey) == Some(role),
    }
}

// ---------------------------------------------------------------------------
// Mention preflight (`validateMentions`)

/// Every notified recipient must be a current member of a channel the
/// viewer can write to. Checked against the signed roster in discovery.
pub fn validate_mentions(
    channel: Option<&ChannelSummary>,
    can_participate: bool,
    recipients: &[String],
) -> Result<(), String> {
    if recipients.is_empty() {
        return Ok(());
    }
    let Some(channel) = channel.filter(|c| can_participate && !c.archived && !c.members.is_empty())
    else {
        return Err("Refresh channel membership before mentioning anyone".into());
    };
    if recipients
        .iter()
        .any(|key| channel.members.binary_search(key).is_err())
    {
        return Err(
            "A selected recipient is no longer a channel member; remove them or refresh membership"
                .into(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// People search (§5g.6) and name matching

/// A hex or `npub1` public key, as typed.
pub fn exact_key(text: &str) -> Option<String> {
    let text = text.trim();
    if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(text.to_ascii_lowercase());
    }
    text.starts_with("npub1")
        .then(|| PublicKey::from_bech32(text).ok())
        .flatten()
        .map(|key| key.to_hex())
}

/// One page of the add-member search: an exact key, or a ranked prefix
/// search of profiles. `page` starts at 1.
pub fn search_filter(query: &str, page: usize) -> Result<Filter, String> {
    let text = query.trim();
    if text.is_empty() || text.encode_utf16().count() > MAX_QUERY || page == 0 {
        return Err("Enter a name or public key to search.".into());
    }
    Ok(match exact_key(text) {
        Some(key) => {
            filter(json!({"kinds": [kinds::PROFILE], "authors": [key], "limit": SEARCH_PAGE}))
        }
        None => filter(json!({
            "kinds": [kinds::PROFILE],
            "search": text,
            "search_mode": "prefix",
            "page": page,
            "limit": SEARCH_PAGE,
        })),
    })
}

/// A person found by a profile search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub pubkey: String,
    pub profile: Profile,
}

/// The newest profile per author, ranked by how the name matches `query`,
/// then by name and key. `exact` keeps only that author.
/// Each key's newest profile, where the relay first listed it
/// (`foldProfiles`).
pub fn fold_people(events: &[Verified]) -> Vec<Person> {
    let mut order: Vec<String> = Vec::new();
    let mut newest: HashMap<String, &Verified> = HashMap::new();
    for event in events.iter().filter(|e| e.kind_u16() == kinds::PROFILE) {
        let key = event.pubkey_hex();
        match newest.get(&key) {
            None => {
                order.push(key.clone());
                newest.insert(key, event);
            }
            Some(current) if crate::event::is_newer(event, current) => {
                newest.insert(key, event);
            }
            Some(_) => {}
        }
    }
    order
        .into_iter()
        .filter_map(|pubkey| {
            let event = newest.get(&pubkey)?;
            Some(Person {
                profile: Profile::from_event(event),
                pubkey,
            })
        })
        .collect()
}

pub fn rank_people(events: &[Verified], query: &str, exact: Option<&str>) -> Vec<Person> {
    let mut newest: HashMap<String, &Verified> = HashMap::new();
    for event in events.iter().filter(|e| e.kind_u16() == kinds::PROFILE) {
        let key = event.pubkey_hex();
        if exact.is_some_and(|exact| exact != key) {
            continue;
        }
        let keep = newest
            .get(&key)
            .is_none_or(|current| crate::event::is_newer(event, current));
        if keep {
            newest.insert(key, event);
        }
    }
    let mut people: Vec<Person> = newest
        .into_iter()
        .map(|(pubkey, event)| Person {
            pubkey,
            profile: Profile::from_event(event),
        })
        .collect();
    let score = |name: &str| match_tier(name, query).unwrap_or(4);
    people.sort_by(|a, b| {
        score(&a.profile.name)
            .cmp(&score(&b.profile.name))
            .then_with(|| compare_names(&a.profile.name, &b.profile.name))
            .then_with(|| a.pubkey.cmp(&b.pubkey))
    });
    people
}

fn compare_names(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// Combining marks `foldName` strips after NFKD: the diacritical mark
/// blocks only, so vowel signs and dakuten survive.
fn is_accent(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe20}'..='\u{fe2f}'
    )
}

/// Accent- and case-folded, one code point at a time (`foldName`): NFKD,
/// diacritics dropped, lowercased, final sigma as sigma.
pub fn fold_name(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let mut folded = String::with_capacity(text.len());
    for c in text.chars() {
        for part in std::iter::once(c).nfkd().filter(|c| !is_accent(*c)) {
            folded.extend(
                part.to_lowercase()
                    .map(|lower| if lower == 'ς' { 'σ' } else { lower }),
            );
        }
    }
    folded
}

/// Folded and trimmed, for comparing names (`normalizeName`).
pub fn normalize_name(text: &str) -> String {
    fold_name(text).trim().to_owned()
}

/// `matchPerson` tiers: 0 equal, 1 the name starts with the query, 2 a later
/// word equals it, 3 a later word starts with it. An empty query is 0.
pub fn match_tier(name: &str, query: &str) -> Option<u8> {
    let needle = normalize_name(query);
    if needle.is_empty() {
        return Some(0);
    }
    let name = normalize_name(name);
    if name == needle {
        return Some(0);
    }
    if name.starts_with(&needle) {
        return Some(1);
    }
    if needle.chars().any(char::is_whitespace) {
        return None;
    }
    let word = |c: char| c.is_alphanumeric();
    let mut best = None;
    let chars: Vec<(usize, char)> = name.char_indices().collect();
    for (i, &(at, c)) in chars.iter().enumerate().skip(1) {
        if !word(c) || word(chars[i - 1].1) || !name[at..].starts_with(&needle) {
            continue;
        }
        let end = at + needle.len();
        if name[end..].chars().next().is_none_or(|next| !word(next)) {
            return Some(2);
        }
        best = Some(3);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::ChannelState;
    use crate::testing::{CHANNEL, Signer, T0, key};

    fn state(signer: &Signer, members: &[(&str, &str)], admins: &[(&str, &str)]) -> ChannelState {
        let mut roster: Vec<Vec<&str>> = vec![vec!["d", CHANNEL]];
        roster.extend(members.iter().map(|(k, r)| vec!["p", *k, "", *r]));
        let mut elevated: Vec<Vec<&str>> = vec![vec!["d", CHANNEL]];
        elevated.extend(admins.iter().map(|(k, r)| vec!["p", *k, *r]));
        let roster: Vec<&[&str]> = roster.iter().map(Vec::as_slice).collect();
        let elevated: Vec<&[&str]> = elevated.iter().map(Vec::as_slice).collect();
        let events = vec![
            signer.relay(
                kinds::CHANNEL_METADATA,
                T0,
                &[
                    &["d", CHANNEL],
                    &["name", "general"],
                    &["public"],
                    &["t", "stream"],
                ],
            ),
            signer.relay(kinds::CHANNEL_ADMINS, T0, &elevated),
            signer.relay(kinds::CHANNEL_MEMBERS, T0, &roster),
        ];
        ChannelState::parse(&events, CHANNEL, &signer.relay_hex()).unwrap()
    }

    #[test]
    fn admins_change_roles_but_not_owners_or_themselves() {
        let signer = Signer::new();
        let viewer = signer.viewer_hex();
        let (bob, carol) = (key(2), key(3));
        let state = state(
            &signer,
            &[(&viewer, "admin"), (&bob, "member"), (&carol, "owner")],
            &[(&viewer, "admin"), (&carol, "owner")],
        );
        let promote = MemberChange {
            pubkey: bob.clone(),
            expected: MemberRole::Member,
            change: Change::Admin,
        };
        assert!(authorize(&state, &viewer, &promote, None).is_ok());
        let stale = MemberChange {
            expected: MemberRole::Guest,
            ..promote.clone()
        };
        assert!(authorize(&state, &viewer, &stale, None).is_err());
        let owner = MemberChange {
            pubkey: carol,
            expected: MemberRole::Owner,
            change: Change::Remove,
        };
        assert!(authorize(&state, &viewer, &owner, None).is_err());
        let event = administration(CHANNEL, &promote, &viewer, 7).unwrap();
        assert_eq!(event.kind, 9000);
        assert_eq!(
            event.tags,
            vec![vec!["h", CHANNEL], vec!["p", &bob], vec!["role", "admin"]]
        );
        let remove = MemberChange {
            change: Change::Remove,
            ..promote
        };
        let event = administration(CHANNEL, &remove, &viewer, 7).unwrap();
        assert_eq!((event.kind, event.tags.len()), (9001, 2));
        assert!(!change_confirmed(&state, &remove));
        assert!(change_confirmed(
            &state,
            &MemberChange {
                change: Change::Member,
                ..remove
            }
        ));
    }

    #[test]
    fn owners_of_an_agent_may_remove_it() {
        let signer = Signer::new();
        let viewer = signer.viewer_hex();
        let agent = key(4);
        let state = state(
            &signer,
            &[(&viewer, "member"), (&agent, "bot"), (&key(5), "owner")],
            &[(&key(5), "owner")],
        );
        let remove = MemberChange {
            pubkey: agent,
            expected: MemberRole::Bot,
            change: Change::Remove,
        };
        assert!(authorize(&state, &viewer, &remove, None).is_err());
        assert!(authorize(&state, &viewer, &remove, Some(&viewer)).is_ok());
        let promote = MemberChange {
            change: Change::Admin,
            ..remove
        };
        assert!(authorize(&state, &viewer, &promote, Some(&viewer)).is_err());
        let read = owner_filter(&key(4));
        assert_eq!(read["authors"], json!([key(4)]));
        assert_eq!(read["consistency"], json!("strong"));
    }

    #[test]
    fn additions_tag_agents_as_bots() {
        let draft = add(CHANNEL, &key(2), true).unwrap();
        assert_eq!(draft.kind, 9000);
        assert_eq!(draft.tags[2], vec!["role", "bot"]);
        assert_eq!(add(CHANNEL, &key(2), false).unwrap().tags.len(), 2);
        assert!(add("general", &key(2), false).is_err());
    }

    #[test]
    fn only_live_streams_and_forums_take_members() {
        let mut channel = ChannelSummary {
            id: CHANNEL.into(),
            channel_type: Some(ChannelType::Stream),
            visibility: Some(Visibility::Public),
            ..Default::default()
        };
        assert!(can_add_members(&channel, true));
        assert!(!can_add_members(&channel, false));
        channel.read_only = true;
        assert!(can_add_members(&channel, false), "public preview");
        channel.archived = true;
        assert!(!can_add_members(&channel, true));
        channel.archived = false;
        channel.channel_type = Some(ChannelType::Dm);
        assert!(!can_add_members(&channel, true));
    }

    #[test]
    fn sessions_share_only_privately_with_members() {
        let mut channel = ChannelSummary {
            id: CHANNEL.into(),
            channel_type: Some(ChannelType::Session),
            private: true,
            members: vec![key(1)],
            ..Default::default()
        };
        assert!(can_share_session(&channel, &key(1)));
        assert!(!can_share_session(&channel, &key(2)));
        assert!(!can_add_members(&channel, true), "never ordinary Members");
        channel.cached = true;
        assert!(!can_share_session(&channel, &key(1)));
    }

    #[test]
    fn mentions_need_current_members() {
        let mut members = vec![key(1), key(2)];
        members.sort();
        let channel = ChannelSummary {
            id: CHANNEL.into(),
            members,
            ..Default::default()
        };
        assert!(validate_mentions(Some(&channel), true, &[key(2)]).is_ok());
        assert!(validate_mentions(Some(&channel), true, &[key(3)]).is_err());
        assert!(validate_mentions(Some(&channel), false, &[key(2)]).is_err());
        assert!(validate_mentions(None, false, &[]).is_ok());
    }

    #[test]
    fn searches_by_key_or_prefix() {
        let hex = key(2);
        let f = search_filter(&hex.to_uppercase(), 1).unwrap();
        assert_eq!(f["authors"], json!([hex]));
        assert!(!f.contains_key("search"));
        let f = search_filter(" ali ", 2).unwrap();
        assert_eq!(f["search"], json!("ali"));
        assert_eq!(f["search_mode"], json!("prefix"));
        assert_eq!(f["page"], json!(2));
        assert_eq!(f["limit"], json!(30));
        assert!(search_filter("  ", 1).is_err());
        let npub =
            nostr::nips::nip19::ToBech32::to_bech32(&PublicKey::from_hex(&hex).unwrap()).unwrap();
        assert_eq!(exact_key(&npub), Some(hex));
    }

    #[test]
    fn folds_accents_case_and_compatibility_forms() {
        assert_eq!(normalize_name("  Ångström "), "angstrom");
        assert_eq!(fold_name("ＡＢＣ"), "abc");
        assert_eq!(fold_name("ΟΔΥΣΣΕΥΣ"), "οδυσσευσ");
        assert_eq!(fold_name("ὀδυσσεύς"), "οδυσσευσ");
        // Marks outside the diacritical blocks are kept.
        assert_eq!(fold_name("हिंदी"), "हिंदी");
        assert_eq!(fold_name("ガ"), "\u{30ab}\u{3099}");
    }

    #[test]
    fn ranks_names_by_match_tier() {
        assert_eq!(match_tier("Alice", "alice"), Some(0));
        assert_eq!(match_tier("Alice Smith", "ali"), Some(1));
        assert_eq!(match_tier("Mary-Alice", "alice"), Some(2));
        assert_eq!(match_tier("Mary Alicia", "ali"), Some(3));
        assert_eq!(match_tier("Malice", "ali"), None);
        assert_eq!(match_tier("Mary Alice", "ry al"), None);
        assert_eq!(match_tier("José Ñúñez", "jose"), Some(1));
        assert_eq!(match_tier("Zoë", "ZOE"), Some(0));
        assert_eq!(match_tier("Ana Sofía", "sofia"), Some(2));

        let signer = Signer::new();
        let a = signer.sign(&signer.viewer, 0, T0, r#"{"name":"Mary Alice"}"#, &[]);
        let b = signer.sign(&signer.relay, 0, T0, r#"{"name":"Alice"}"#, &[]);
        let people = rank_people(&[a, b], "alice", None);
        assert_eq!(people[0].profile.name, "Alice");
        assert_eq!(people[1].profile.name, "Mary Alice");
    }
}
