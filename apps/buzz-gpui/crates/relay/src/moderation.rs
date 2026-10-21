//! Community administration (NIP-43, buzz-app `bundled/moderation`): roles
//! from the relay-signed member list (kind 13534, `communities/roster.ts`),
//! the owner and admin member commands (kinds 9030, 9031 and 9032,
//! `communities/admin-protocol.ts`) and invite links (`POST /api/invites`).
//!
//! Roles here only shape the UI; the relay decides every privileged change.

use crate::event::{Verified, is_hex64, kinds, newest};
use crate::http::HttpError;
use crate::messages::EventTemplate;
use crate::socket::{Filter, filter};
use nostr::prelude::EventId;
use serde_json::{Value, json};

pub const MEMBERSHIP_KIND: u16 = kinds::COMMUNITY_ROSTER;
const ADD_KIND: u16 = 9030;
const REMOVE_KIND: u16 = 9031;
const ROLE_KIND: u16 = 9032;

/// Invite expiry bounds in seconds (`inviteRequest`).
pub const MIN_TTL: u64 = 60;
pub const MAX_TTL: u64 = 30 * 24 * 60 * 60;
pub const MAX_USES: u32 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    /// The capitalized role a roster row shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Owner => "Owner",
            Self::Admin => "Admin",
            Self::Member => "Member",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub pubkey: String,
    pub role: Role,
}

/// Whether a role may invite people and manage members (`managesCommunity`).
pub fn manages(role: Option<Role>) -> bool {
    matches!(role, Some(Role::Owner | Role::Admin))
}

/// The newest roster read, NIP-01 order (`readRoster`, `createCommunityMembership`).
pub fn roster_filter(relay_author: &str) -> Filter {
    filter(json!({"kinds": [MEMBERSHIP_KIND], "authors": [relay_author], "limit": 1}))
}

/// Members from the relay-signed snapshot (`["member", pubkey, role]`
/// tags): the first entry per key wins, an unknown role reads as member,
/// and a key that is not lowercase hex is skipped. Anyone else's
/// signature is refused.
pub fn members_from_snapshot(
    event: &nostr::prelude::Event,
    relay_author: &str,
) -> Result<Vec<Member>, String> {
    if event.kind.as_u16() != MEMBERSHIP_KIND || event.pubkey.to_hex() != relay_author {
        return Err("Member list is not signed by this community".into());
    }
    let mut members: Vec<Member> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for tag in event.tags.iter() {
        let tag = tag.as_slice();
        if tag.first().map(String::as_str) != Some("member") {
            continue;
        }
        let Some(pubkey) = tag.get(1).filter(|key| is_hex64(key)) else {
            continue;
        };
        if !seen.insert(pubkey) {
            continue;
        }
        let role = tag
            .get(2)
            .and_then(|role| Role::parse(role))
            .unwrap_or(Role::Member);
        members.push(Member {
            pubkey: pubkey.clone(),
            role,
        });
    }
    Ok(members)
}

/// The member list from a roster read: `None` when the community publishes
/// none, so it has no roles to derive.
pub fn roster(events: &[Verified], relay_author: &str) -> Result<Option<Vec<Member>>, String> {
    newest(events)
        .map(|event| members_from_snapshot(event, relay_author))
        .transpose()
}

/// A change an owner or admin can take on a roster row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Promote,
    Demote,
    Remove,
}

impl Action {
    /// The menu item, confirmation title and button.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Promote => "Make admin",
            Self::Demote => "Make member",
            Self::Remove => "Remove",
        }
    }

    /// What confirming it means.
    pub fn description(self) -> &'static str {
        match self {
            Self::Remove => "They lose access to this community until they are invited again.",
            Self::Promote => "Admins can invite people and remove members.",
            Self::Demote => "They will no longer be able to invite people or manage members.",
        }
    }

    pub fn change(self, pubkey: &str) -> Change {
        let pubkey = pubkey.to_owned();
        match self {
            Self::Promote => Change::Role {
                pubkey,
                role: Grant::Admin,
            },
            Self::Demote => Change::Role {
                pubkey,
                role: Grant::Member,
            },
            Self::Remove => Change::Remove { pubkey },
        }
    }
}

/// Actions the relay's permission matrix would allow `actor` to take on
/// `target` (`allowedActions`).
pub fn allowed_actions(actor: Option<Role>, target: &Member, own: bool) -> Vec<Action> {
    if own || target.role == Role::Owner {
        return Vec::new();
    }
    match actor {
        Some(Role::Admin) if target.role == Role::Member => vec![Action::Remove],
        Some(Role::Owner) if target.role == Role::Admin => vec![Action::Demote, Action::Remove],
        Some(Role::Owner) => vec![Action::Promote, Action::Remove],
        _ => Vec::new(),
    }
}

/// A role a member command can grant; never owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    Admin,
    Member,
}

impl Grant {
    fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }
}

/// One member command (`MemberChange`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Add { pubkey: String, role: Grant },
    Role { pubkey: String, role: Grant },
    Remove { pubkey: String },
}

/// The unsigned NIP-43 command for one member (`memberCommand`).
pub fn member_command(change: &Change, now: u64) -> Result<EventTemplate, String> {
    let (kind, pubkey, role) = match change {
        Change::Add { pubkey, role } => (ADD_KIND, pubkey, Some(*role)),
        Change::Role { pubkey, role } => (ROLE_KIND, pubkey, Some(*role)),
        Change::Remove { pubkey } => (REMOVE_KIND, pubkey, None),
    };
    if !is_hex64(pubkey) {
        return Err("Invalid member change".into());
    }
    let mut tags = vec![vec!["p".to_owned(), pubkey.clone()]];
    if let Some(role) = role {
        tags.push(vec!["role".to_owned(), role.as_str().to_owned()]);
    }
    Ok(EventTemplate {
        kind,
        created_at: now,
        tags,
        content: String::new(),
    })
}

/// Exact relay refusals worth showing verbatim (`adminReason`: buzz-relay
/// `relay_admin.rs`, `ingest.rs`, `api/invites.rs`).
const REASONS: &[&str] = &[
    "invalid: actor not authorized: must be admin or owner",
    "invalid: actor not authorized: must be owner",
    "invalid: actor not authorized: only owner can grant admin role",
    "invalid: actor not authorized: admins can only remove members",
    "invalid: cannot remove yourself",
    "invalid: cannot remove the relay owner",
    "invalid: cannot change your own role",
    "invalid: cannot change the relay owner's role",
    "invalid: cannot set role to owner",
    "blocked: you are banned from this community",
    "only relay owners and admins can create invites",
    "community writes are temporarily unavailable",
];

/// A relay refusal worth showing verbatim; anything else stays generic.
pub fn admin_reason(reason: &str) -> Option<&str> {
    let member_not_found = reason
        .strip_prefix("invalid: member not found: ")
        .is_some_and(is_hex64);
    let bounds = ["ttl_secs", "max_uses"].iter().any(|field| {
        reason
            .strip_prefix(field)
            .and_then(|rest| rest.strip_prefix(" must be between "))
            .and_then(|rest| rest.split_once(" and "))
            .is_some_and(|(low, high)| {
                [low, high]
                    .iter()
                    .all(|n| (1..=8).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()))
            })
    });
    (REASONS.contains(&reason) || member_not_found || bounds).then_some(reason)
}

/// Exact invite admission refusal codes (buzz-relay `api/invites.rs`).
const ADMISSION: &[&str] = &[
    "invite_expired",
    "invite_exhausted",
    "invite_invalid",
    "join_policy_required",
    "join_policy_not_accepted",
];

/// `readResponse`: the JSON object a community request answered, or its
/// failure. A refusal shows verbatim only when `refusal` keeps it.
fn read_response(
    result: Result<Vec<u8>, HttpError>,
    refusal: impl Fn(&str) -> Option<&str>,
) -> Result<serde_json::Map<String, Value>, String> {
    let body = match result {
        Ok(body) => body,
        Err(HttpError::Status {
            status, message, ..
        }) => {
            return Err(refusal(&message)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Community request failed ({status})")));
        }
        Err(error) => return Err(error.to_string()),
    };
    match serde_json::from_slice(&body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err("Invalid community response".into()),
    }
}

/// The receipt of a member command (`changeMember`): it counts only when it
/// names this event, and a refusal shows the relay's allowed reason.
pub fn member_outcome(result: Result<Vec<u8>, HttpError>, id: &EventId) -> Result<(), String> {
    let receipt = read_response(result, admin_reason)?;
    let accepted = receipt.get("accepted").and_then(Value::as_bool);
    if receipt.get("event_id").and_then(Value::as_str) != Some(&id.to_hex()) || accepted.is_none() {
        return Err("Member change could not be confirmed".into());
    }
    if accepted == Some(true) {
        return Ok(());
    }
    Err(receipt
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or("The relay did not accept the change")
        .to_owned())
}

/// The `POST /api/invites` body (`inviteRequest`).
pub fn invite_request(ttl_secs: u64, max_uses: Option<u32>) -> Result<String, String> {
    if !(MIN_TTL..=MAX_TTL).contains(&ttl_secs) {
        return Err("Invalid invite expiry".into());
    }
    if max_uses.is_some_and(|uses| !(1..=MAX_USES).contains(&uses)) {
        return Err("Invalid invite use limit".into());
    }
    Ok(json!({"ttl_secs": ttl_secs, "max_uses": max_uses}).to_string())
}

/// A minted invite link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub code: String,
    pub url: String,
    pub expires_at: Option<u64>,
    pub max_uses: Option<u64>,
    pub uses_remaining: Option<u64>,
}

/// The answer to `POST /api/invites`. Only admission codes show verbatim,
/// as in buzz-app's native adapter.
pub fn invite_outcome(result: Result<Vec<u8>, HttpError>) -> Result<Invite, String> {
    let body = read_response(result, |message| {
        ADMISSION.iter().copied().find(|code| *code == message)
    })?;
    let text = |field: &str| body.get(field).and_then(Value::as_str).map(str::to_owned);
    let number = |field: &str| body.get(field).and_then(Value::as_u64);
    match (text("code"), text("url")) {
        (Some(code), Some(url)) if !url.is_empty() => Ok(Invite {
            code,
            url,
            expires_at: number("expires_at"),
            max_uses: number("max_uses"),
            uses_remaining: number("uses_remaining"),
        }),
        _ => Err("Invalid community response".into()),
    }
}

/// An exact public identity: an npub or 64 hex characters, lowercased.
pub fn parse_public_key(input: &str) -> Result<String, String> {
    crate::members::exact_key(input)
        .ok_or_else(|| "Enter an npub or 64-character hex public key.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Signer;
    use nostr::prelude::{Keys, ToBech32};

    const OWNER: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn key(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn snapshot(signer: &Signer, keys: &Keys, tags: &[&[&str]]) -> Verified {
        signer.sign(keys, MEMBERSHIP_KIND, 1, "", tags)
    }

    #[test]
    fn reads_roles_only_from_the_relay_signed_member_list() {
        let signer = Signer::new();
        let (relay, author) = (&signer.relay, signer.relay_hex());
        let (admin, member) = (key('1'), key('2'));
        let roster: &[&[&str]] = &[
            &["-"],
            &["member", OWNER, "owner"],
            &["member", &admin, "admin"],
            &["member", &member, "member"],
        ];
        assert_eq!(
            members_from_snapshot(&snapshot(&signer, relay, roster), &author).unwrap(),
            [
                Member {
                    pubkey: OWNER.into(),
                    role: Role::Owner
                },
                Member {
                    pubkey: admin.clone(),
                    role: Role::Admin
                },
                Member {
                    pubkey: member.clone(),
                    role: Role::Member
                },
            ]
        );
        // Signed by anyone else: refused.
        assert!(
            members_from_snapshot(&snapshot(&signer, &signer.viewer, roster), &author).is_err()
        );
        // Other tags and uppercase keys are not members.
        let upper = "A".repeat(64);
        let odd: &[&[&str]] = &[&["p", &admin, "", "owner"], &["member", &upper, "owner"]];
        assert!(
            members_from_snapshot(&snapshot(&signer, relay, odd), &author)
                .unwrap()
                .is_empty()
        );
        // The first entry wins; an unknown role reads as member.
        let repeated: &[&[&str]] = &[
            &["member", &admin, "boss"],
            &["member", &admin, "owner"],
            &["member", &member],
        ];
        let members = members_from_snapshot(&snapshot(&signer, relay, repeated), &author).unwrap();
        assert_eq!(members.len(), 2);
        assert!(members.iter().all(|m| m.role == Role::Member));
    }

    #[test]
    fn the_newest_roster_wins_and_none_means_no_list() {
        let signer = Signer::new();
        let author = signer.relay_hex();
        assert_eq!(roster(&[], &author), Ok(None));
        let events = [
            signer.relay(MEMBERSHIP_KIND, 10, &[&["member", OWNER, "owner"]]),
            signer.relay(MEMBERSHIP_KIND, 20, &[&["member", OWNER, "admin"]]),
        ];
        let members = roster(&events, &author).unwrap().unwrap();
        assert_eq!(members[0].role, Role::Admin);
    }

    #[test]
    fn offers_only_actions_the_relay_permission_matrix_allows() {
        let m = |role| Member {
            pubkey: key('2'),
            role,
        };
        use Action::*;
        use Role::{Admin, Owner};
        assert_eq!(
            allowed_actions(Some(Owner), &m(Role::Member), false),
            [Promote, Remove]
        );
        assert_eq!(
            allowed_actions(Some(Owner), &m(Admin), false),
            [Demote, Remove]
        );
        assert!(allowed_actions(Some(Owner), &m(Owner), false).is_empty());
        assert!(allowed_actions(Some(Owner), &m(Admin), true).is_empty());
        assert_eq!(
            allowed_actions(Some(Admin), &m(Role::Member), false),
            [Remove]
        );
        assert!(allowed_actions(Some(Admin), &m(Admin), false).is_empty());
        assert!(allowed_actions(Some(Role::Member), &m(Role::Member), false).is_empty());
        assert!(allowed_actions(None, &m(Role::Member), false).is_empty());
        assert!(manages(Some(Owner)) && manages(Some(Admin)));
        assert!(!manages(Some(Role::Member)) && !manages(None));
    }

    #[test]
    fn member_commands_are_nip43_events() {
        let target = key('3');
        let add = member_command(
            &Change::Add {
                pubkey: target.clone(),
                role: Grant::Member,
            },
            5,
        )
        .unwrap();
        assert_eq!(add.kind, 9030);
        assert_eq!(add.created_at, 5);
        assert_eq!(
            add.tags,
            [
                vec!["p".to_owned(), target.clone()],
                vec!["role".into(), "member".into()]
            ]
        );
        let promote = member_command(&Action::Promote.change(&target), 5).unwrap();
        assert_eq!(promote.kind, 9032);
        assert_eq!(promote.tags[1], ["role", "admin"]);
        let demote = member_command(&Action::Demote.change(&target), 5).unwrap();
        assert_eq!(demote.tags[1], ["role", "member"]);
        let remove = member_command(&Action::Remove.change(&target), 5).unwrap();
        assert_eq!(remove.kind, 9031);
        assert_eq!(remove.tags, [vec!["p".to_owned(), target]]);
        assert!(member_command(&Action::Remove.change("Carol"), 5).is_err());
    }

    fn status(status: u16, message: &str) -> Result<Vec<u8>, HttpError> {
        Err(HttpError::Status {
            status,
            message: message.into(),
            retry_after: None,
            not_sent: false,
        })
    }

    #[test]
    fn member_receipts_keep_only_known_refusals() {
        let id = EventId::from_hex(&key('e')).unwrap();
        let receipt = |value: Value| Ok(value.to_string().into_bytes());
        assert_eq!(
            member_outcome(
                receipt(json!({"event_id": id.to_hex(), "accepted": true})),
                &id
            ),
            Ok(())
        );
        let refusal = "invalid: actor not authorized: admins can only remove members";
        assert_eq!(
            member_outcome(status(403, refusal), &id),
            Err(refusal.into())
        );
        assert_eq!(
            member_outcome(status(500, "database: connection reset"), &id),
            Err("Community request failed (500)".into())
        );
        let missing = format!("invalid: member not found: {}", key('a'));
        assert_eq!(member_outcome(status(400, &missing), &id), Err(missing));
        // A receipt naming another event claims nothing.
        assert_eq!(
            member_outcome(
                receipt(json!({"event_id": key('0').replace('0', "1"), "accepted": true})),
                &id
            ),
            Err("Member change could not be confirmed".into())
        );
        assert_eq!(
            member_outcome(
                receipt(json!({"event_id": id.to_hex(), "accepted": false, "message": ""})),
                &id
            ),
            Err("The relay did not accept the change".into())
        );
        assert_eq!(
            member_outcome(Ok(b"<html>".to_vec()), &id),
            Err("Invalid community response".into())
        );
        assert_eq!(
            member_outcome(Err(HttpError::Transport("Relay unreachable".into())), &id),
            Err("could not reach the relay: Relay unreachable".into())
        );
    }

    #[test]
    fn admin_reasons_match_the_reference_patterns() {
        assert!(admin_reason("ttl_secs must be between 60 and 2592000").is_some());
        assert!(admin_reason("max_uses must be between 1 and 10000").is_some());
        assert!(admin_reason("max_uses must be between 1 and 123456789").is_none());
        assert!(admin_reason("invalid: member not found: abc").is_none());
        assert!(admin_reason("only relay owners and admins can create invites").is_some());
    }

    #[test]
    fn invite_requests_are_bounded() {
        let body =
            |ttl, uses| serde_json::from_str::<Value>(&invite_request(ttl, uses).unwrap()).unwrap();
        assert_eq!(
            body(3 * 86_400, None),
            json!({"ttl_secs": 259_200, "max_uses": null})
        );
        assert_eq!(
            body(86_400, Some(25)),
            json!({"ttl_secs": 86_400, "max_uses": 25})
        );
        assert!(invite_request(59, None).is_err());
        assert!(invite_request(MAX_TTL + 1, None).is_err());
        assert!(invite_request(600, Some(0)).is_err());
        assert!(invite_request(600, Some(MAX_USES + 1)).is_err());
    }

    #[test]
    fn invites_read_the_minted_link() {
        let body = json!({
            "code": "abc",
            "url": "http://127.0.0.1:53000/invite/abc",
            "expires_at": 1_700_003_600,
            "max_uses": null,
            "uses_remaining": null,
        });
        let invite = invite_outcome(Ok(body.to_string().into_bytes())).unwrap();
        assert_eq!(invite.url, "http://127.0.0.1:53000/invite/abc");
        assert_eq!(invite.expires_at, Some(1_700_003_600));
        assert_eq!(invite.max_uses, None);
        // Only admission codes are shown verbatim on the native path.
        assert_eq!(
            invite_outcome(status(
                403,
                "only relay owners and admins can create invites"
            )),
            Err("Community request failed (403)".into())
        );
        assert_eq!(
            invite_outcome(status(403, "invite_invalid")),
            Err("invite_invalid".into())
        );
        assert_eq!(
            invite_outcome(Ok(br#"{"code":"abc"}"#.to_vec())),
            Err("Invalid community response".into())
        );
    }

    #[test]
    fn accepts_only_an_exact_identity() {
        let keys = Keys::generate();
        let hex = keys.public_key().to_hex();
        let npub = keys.public_key().to_bech32().unwrap();
        assert_eq!(parse_public_key(&format!("  {npub} ")), Ok(hex.clone()));
        assert_eq!(parse_public_key(&hex.to_uppercase()), Ok(hex));
        let error = Err("Enter an npub or 64-character hex public key.".to_owned());
        assert_eq!(parse_public_key("Carol"), error);
        assert_eq!(parse_public_key(&"c".repeat(63)), error);
        assert_eq!(parse_public_key("nsec1xyz"), error);
    }
}
