//! Channel lifecycle and details (`docs/port/relay.md` §5m): create (9007),
//! join (9021), leave (9022), delete (9008), archive and unarchive (9002
//! `archived`), details edits (9002 name/about/visibility/ttl) and the DM
//! hide command (41012).
//!
//! Pure: templates, validation and the permission rules read from one
//! strong read of a channel's 39000/39001/39002 ([`ChannelState`]). The
//! session runs the reads, publishes, and polls for confirmation at
//! [`CONFIRM_DELAYS`].
//!
//! Ports `channel-lifecycle-protocol.ts`, `channel-details-protocol.ts`
//! and the 9007 template of `work-sessions.ts`.

use crate::discovery::{Visibility, channel_visibility, is_uuid, session_metadata};
use crate::event::{Verified, is_newer, kinds};
use crate::members::is_key;
use crate::messages::{Draft, EventTemplate};
use crate::socket::{Filter, filter};
use nostr::prelude::{EventId, PublicKey};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

/// Confirmation polls after a lifecycle command (`channel-lifecycle.ts`).
pub const CONFIRM_DELAYS: [Duration; 4] = [
    Duration::from_millis(0),
    Duration::from_millis(250),
    Duration::from_millis(750),
    Duration::from_millis(1500),
];
/// Overall budget for one lifecycle or administration run.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(20);

pub const MAX_NAME_CHARS: usize = 120;
pub const MAX_ABOUT_CHARS: usize = 1000;
/// The longest channel duration, in seconds: a positive i32.
pub const MAX_TTL: u64 = 2_147_483_647;
/// A new temporary channel cleans up after a week without activity
/// (`DEFAULT_TEMPORARY_CHANNEL_TTL_SECONDS`).
pub const DEFAULT_TEMPORARY_TTL: u64 = 7 * 24 * 60 * 60;

const NAME_ERROR: &str = "Enter a channel name of 1–120 characters without a leading #.";
const ABOUT_LENGTH_ERROR: &str = "Use a description of at most 1,000 characters.";
const ABOUT_SESSION_ERROR: &str =
    "Remove \"Buzz session (\" from the description; that text is used by Buzz for work sessions.";
const TTL_ERROR: &str = "Channel duration could not be verified.";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LifecycleError(pub String);

/// Why a lifecycle command failed (`ChannelLifecycleUnconfirmed`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActionError {
    /// Nothing was published, or the relay refused it.
    #[error("{0}")]
    Failed(String),
    /// The command may have reached the relay. Resending could repeat it,
    /// so the dialog that sent it stays unable to send it again.
    #[error(
        "The request may have taken effect. Close this dialog and refresh channels before trying again. {0}"
    )]
    Unconfirmed(String),
}

fn error(message: impl Into<String>) -> LifecycleError {
    LifecycleError(message.into())
}

/// `lifecycleChannelId`: a channel ID, which is a UUID.
pub fn channel_id(value: &str) -> Result<&str, LifecycleError> {
    if is_uuid(value) {
        Ok(value)
    } else {
        Err(error("Invalid channel ID"))
    }
}

// ---------------------------------------------------------------------------
// Channel state: one strong read of 39000/39001/39002

/// A member's role in the relay-signed 39002 (`channel.rs:108-136`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MemberRole {
    Owner,
    Admin,
    Member,
    Guest,
    Bot,
    /// Missing or not a role this client knows.
    Unknown,
}

impl MemberRole {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("owner") => Self::Owner,
            Some("admin") => Self::Admin,
            Some("member") => Self::Member,
            Some("guest") => Self::Guest,
            Some("bot") => Self::Bot,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
            Self::Guest => "guest",
            Self::Bot => "bot",
            Self::Unknown => "unknown",
        }
    }

    pub fn elevated(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }
}

/// The NIP-29 `t` type a lifecycle read requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateType {
    Stream,
    Forum,
    Dm,
}

/// The exact, validated state of one channel, read strongly from the relay
/// (`lifecycleSettings`, `memberAuthority`, `detailsSettings`).
#[derive(Clone, Debug)]
pub struct ChannelState {
    pub channel: String,
    /// The 39000 event id: the optimistic version of the details.
    pub version: EventId,
    pub channel_type: StateType,
    pub archived: bool,
    /// The exact `name` tag.
    pub name: String,
    pub about: String,
    pub visibility: Option<Visibility>,
    /// Absent means the channel is permanent.
    pub ttl: Option<u64>,
    /// NIP-CL labels: the `t` tags after the type.
    pub labels: Vec<String>,
    /// Members and their 39002 roles.
    pub roles: BTreeMap<String, MemberRole>,
    /// The 39000 and 39002, for discovery.
    pub metadata: Verified,
    pub roster: Verified,
}

/// The strong exact read of one channel's state (`administration.ts:72-91`).
pub fn state_filters(relay_author: &str, channel: &str) -> Vec<Filter> {
    [
        kinds::CHANNEL_METADATA,
        kinds::CHANNEL_ADMINS,
        kinds::CHANNEL_MEMBERS,
    ]
    .into_iter()
    .map(|kind| state_filter(kind, relay_author, channel))
    .collect()
}

/// One record of [`state_filters`].
pub fn state_filter(kind: u16, relay_author: &str, channel: &str) -> Filter {
    filter(json!({
        "kinds": [kind],
        "consistency": "strong",
        "authors": [relay_author],
        "#d": [channel],
        "limit": 1,
    }))
}

/// The admin keys of a relay-signed 39001 on its own, for the fold's
/// moderator deletes ([`crate::fold::FoldOptions::channel_admins`]).
/// `None` when the event is not this channel's 39001 from the relay or an
/// entry is malformed; the 39002 cross-check needs [`ChannelState`].
pub fn channel_admins(
    event: &Verified,
    channel: &str,
    relay_author: &str,
) -> Option<HashSet<PublicKey>> {
    let events = std::slice::from_ref(event);
    let admins = record(events, kinds::CHANNEL_ADMINS, channel, relay_author).ok()??;
    elevated_members(admins)?
        .keys()
        .map(|key| PublicKey::from_hex(key).ok())
        .collect()
}

/// The single value of a tag that may appear at most once, with exactly
/// one value (`exactLifecycleTag`).
fn exact_tag<'a>(event: &'a Verified, name: &str) -> Result<Option<&'a str>, LifecycleError> {
    let mut matches = event
        .tags
        .iter()
        .filter(|tag| tag.kind() == name)
        .map(|tag| tag.as_slice());
    let first = matches.next();
    if matches.next().is_some() || first.is_some_and(|tag| tag.len() != 2) {
        return Err(error(format!("Malformed channel {name} state")));
    }
    Ok(first.map(|tag| tag[1].as_str()))
}

/// The channel type of relay-signed channel state (`channelTypeTag`).
/// NIP-CL puts the type in the first `t` tag; later `t` tags are channel
/// labels, not a second type.
fn channel_type_tag(event: &Verified) -> Result<Option<&str>, LifecycleError> {
    let first = event.tags.iter().find(|tag| tag.kind() == "t");
    match first.map(|tag| tag.as_slice()) {
        Some(tag) if tag.len() != 2 => Err(error("Malformed channel t state")),
        Some(tag) => Ok(Some(tag[1].as_str())),
        None => Ok(None),
    }
}

fn record<'a>(
    events: &'a [Verified],
    kind: u16,
    channel: &str,
    relay_author: &str,
) -> Result<Option<&'a Verified>, LifecycleError> {
    let mut selected: Option<&Verified> = None;
    for event in events.iter().filter(|event| event.kind_u16() == kind) {
        if event.pubkey_hex() != relay_author || exact_tag(event, "d")? != Some(channel) {
            return Err(error("Channel state did not match the requested authority"));
        }
        if selected.is_none_or(|current| is_newer(event, current)) {
            selected = Some(event);
        }
    }
    Ok(selected)
}

/// The 39001's entries: each exactly `["p", key, "owner"|"admin"]`, once.
fn elevated_members(admins: &Verified) -> Option<BTreeMap<String, MemberRole>> {
    let mut elevated = BTreeMap::new();
    for entry in admins.tags_named("p") {
        let key = entry.get(1).map(String::as_str).unwrap_or_default();
        let role = MemberRole::parse(entry.get(2).map(String::as_str));
        if entry.len() != 3 || !is_key(key) || !role.elevated() || elevated.contains_key(key) {
            return None;
        }
        elevated.insert(key.to_owned(), role);
    }
    Some(elevated)
}

/// A relay TTL must be an unambiguous positive i32.
fn parse_ttl(value: Option<&str>) -> Result<Option<u64>, LifecycleError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let invalid = || error("Channel duration could not be verified.");
    if value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let ttl: u64 = value.parse().map_err(|_| invalid())?;
    if ttl == 0 || ttl > MAX_TTL {
        return Err(invalid());
    }
    Ok(Some(ttl))
}

/// The channel's metadata, admins or roster record is missing.
pub const UNVERIFIED: &str =
    "Current channel permissions could not be verified. Refresh and try again.";

impl ChannelState {
    /// Validates the answer to [`state_filters`]. The 39001 must list
    /// exactly the elevated members of the 39002, with the same roles.
    pub fn parse(
        events: &[Verified],
        channel: &str,
        relay_author: &str,
    ) -> Result<Self, LifecycleError> {
        if events.iter().any(|event| {
            !matches!(
                event.kind_u16(),
                kinds::CHANNEL_METADATA | kinds::CHANNEL_ADMINS | kinds::CHANNEL_MEMBERS
            )
        }) {
            return Err(error("Unexpected channel membership response"));
        }
        let metadata = record(events, kinds::CHANNEL_METADATA, channel, relay_author)?;
        let admins = record(events, kinds::CHANNEL_ADMINS, channel, relay_author)?;
        let roster = record(events, kinds::CHANNEL_MEMBERS, channel, relay_author)?;
        let (Some(metadata), Some(admins), Some(roster)) = (metadata, admins, roster) else {
            return Err(error(UNVERIFIED));
        };
        let channel_type = match channel_type_tag(metadata)? {
            Some("stream") => StateType::Stream,
            Some("forum") => StateType::Forum,
            Some("dm") => StateType::Dm,
            _ => return Err(error("Current channel type could not be verified")),
        };
        let archived = match exact_tag(metadata, "archived")? {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(error("Malformed channel archive state")),
        };
        let mut roles = BTreeMap::new();
        for entry in roster.tags_named("p") {
            let key = entry.get(1).map(String::as_str).unwrap_or_default();
            if !(2..=4).contains(&entry.len()) || !is_key(key) || roles.contains_key(key) {
                return Err(error("Malformed channel membership state"));
            }
            roles.insert(
                key.to_owned(),
                MemberRole::parse(entry.get(3).map(String::as_str)),
            );
        }
        let inconsistent = || error("Member roles are inconsistent. Refresh members and roles.");
        let elevated = elevated_members(admins).ok_or_else(inconsistent)?;
        if elevated
            .iter()
            .any(|(key, role)| roles.get(key) != Some(role))
        {
            return Err(inconsistent());
        }
        if roles
            .iter()
            .any(|(key, role)| role.elevated() && elevated.get(key) != Some(role))
        {
            return Err(inconsistent());
        }
        Ok(Self {
            channel: channel.to_owned(),
            version: metadata.id,
            channel_type,
            archived,
            name: exact_tag(metadata, "name")?.unwrap_or_default().to_owned(),
            about: exact_tag(metadata, "about")?.unwrap_or_default().to_owned(),
            visibility: channel_visibility(metadata),
            ttl: parse_ttl(exact_tag(metadata, "ttl")?)?,
            labels: crate::channel_labels::identity(metadata.tags.iter().map(|tag| tag.as_slice()))
                .labels,
            roles,
            metadata: metadata.clone(),
            roster: roster.clone(),
        })
    }

    pub fn role(&self, key: &str) -> Option<MemberRole> {
        self.roles.get(key).copied()
    }

    /// Session channels, top-level or child (`sessionMetadata`).
    pub fn is_session(&self) -> bool {
        session_metadata(Some(&self.about)).is_some()
    }

    fn child_session(&self) -> bool {
        session_metadata(Some(&self.about)).is_some_and(|parent| parent.is_some())
    }

    /// A stream or forum that is not archived and not a session: the only
    /// channels whose members are administered.
    fn active(&self) -> bool {
        self.channel_type != StateType::Dm && !self.archived && !self.is_session()
    }

    fn owners(&self) -> usize {
        self.roles
            .values()
            .filter(|role| **role == MemberRole::Owner)
            .count()
    }

    /// The channel's owners, already checked against the 39001.
    pub fn owner_keys(&self) -> Vec<String> {
        self.roles
            .iter()
            .filter(|(_, role)| **role == MemberRole::Owner)
            .map(|(key, _)| key.clone())
            .collect()
    }

    // ----- lifecycle (`lifecycleSettings`) -----

    /// What the viewer may do to the channel as a whole. Refuses when the
    /// viewer is not a member.
    pub fn lifecycle(&self, viewer: &str) -> Result<LifecyclePermissions, LifecycleError> {
        let Some(role) = self.role(viewer) else {
            return Err(error("You are no longer a channel member"));
        };
        let dm = self.channel_type == StateType::Dm;
        let manager = role.elevated();
        let last_owner = role == MemberRole::Owner && self.owners() == 1;
        Ok(LifecyclePermissions {
            can_archive: !dm && !self.archived && manager,
            can_unarchive: !dm && self.archived && manager,
            can_delete: !dm && !self.archived && role == MemberRole::Owner,
            can_leave: !dm && !last_owner,
            can_hide: dm,
            leave_reason: (last_owner && !dm)
                .then(|| "Transfer ownership before leaving the channel.".to_owned()),
            delete_unavailable: false,
        })
    }

    // ----- members (`memberAuthority`) -----

    /// The viewer administers members: an elevated member of an active
    /// channel.
    pub fn can_manage(&self, viewer: &str) -> bool {
        self.active() && self.role(viewer).is_some_and(MemberRole::elevated)
    }

    /// The viewer may change this member's role or remove them.
    pub fn can_manage_member(&self, viewer: &str, key: &str) -> bool {
        self.can_manage(viewer)
            && key != viewer
            && self
                .role(key)
                .is_some_and(|role| !matches!(role, MemberRole::Owner | MemberRole::Unknown))
    }

    /// Removal is also allowed for an agent whose verified owner is the
    /// viewer; ownership never grants role edits.
    pub fn can_remove_member(&self, viewer: &str, key: &str, owner: Option<&str>) -> bool {
        self.can_manage_member(viewer, key)
            || (self.active()
                && self
                    .role(viewer)
                    .is_some_and(|role| role != MemberRole::Unknown)
                && owner == Some(viewer)
                && key != viewer
                && self
                    .role(key)
                    .is_some_and(|role| !matches!(role, MemberRole::Owner | MemberRole::Unknown)))
    }

    // ----- details (`detailsSettings`) -----

    pub fn details(&self, viewer: &str) -> Result<ChannelDetails, LifecycleError> {
        let visibility = self
            .visibility
            .ok_or_else(|| error("Current channel details could not be verified."))?;
        if self.name.is_empty() {
            return Err(error("Current channel details could not be verified."));
        }
        let role = self.role(viewer);
        Ok(ChannelDetails {
            channel: self.channel.clone(),
            version: self.version,
            draft: DetailsDraft {
                name: self.name.clone(),
                description: self.about.clone(),
                private: visibility == Visibility::Private,
                ttl: self.ttl,
                labels: Some(self.labels.clone()),
            },
            can_edit: self.channel_type != StateType::Dm
                && !self.archived
                && !self.child_session()
                && role.is_some_and(MemberRole::elevated),
        })
    }
}

/// What [`ChannelState::lifecycle`] allows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LifecyclePermissions {
    pub can_archive: bool,
    pub can_unarchive: bool,
    pub can_delete: bool,
    pub can_leave: bool,
    pub can_hide: bool,
    /// Why Leave is unavailable, when it is.
    pub leave_reason: Option<String>,
    /// The owner-agent check for Delete could not be read.
    pub delete_unavailable: bool,
}

impl LifecyclePermissions {
    /// Whether an owner agent of the viewer's could grant what a direct
    /// role does not: Unarchive when archived, else Delete, which implies
    /// Archive (`channel-lifecycle.ts:146-163`).
    pub fn wants_owner_evidence(&self, archived: bool) -> bool {
        let action = if archived {
            Action::Unarchive
        } else {
            Action::Delete
        };
        self.wants_evidence_for(archived, action)
    }

    /// Whether owner-agent evidence could grant `action` (archive,
    /// unarchive or delete) that a direct role does not: only when it fits
    /// the archive state.
    pub fn wants_evidence_for(&self, archived: bool, action: Action) -> bool {
        let granted = match action {
            Action::Archive => self.can_archive,
            Action::Unarchive => self.can_unarchive,
            Action::Delete => self.can_delete,
            _ => return false,
        };
        !self.can_hide && archived == (action == Action::Unarchive) && !granted
    }

    /// The relay lets an owner agent's verified human archive, unarchive
    /// and delete as a direct owner would.
    pub fn grant_owned_agent(&mut self, archived: bool) {
        self.can_archive = !archived;
        self.can_unarchive = archived;
        self.can_delete = !archived;
    }
}

/// Owner profiles read per batch for owner-agent evidence.
pub const OWNER_BATCH: usize = 4;
/// The whole owner-agent lookup's budget.
pub const OWNER_EVIDENCE_TIMEOUT: Duration = Duration::from_secs(5);

/// One `{kinds:[0], authors:[owner], limit:1}` per owner.
pub fn owner_profile_filters(owners: &[String]) -> Vec<Filter> {
    owners
        .iter()
        .map(|owner| filter(json!({"kinds": [kinds::PROFILE], "authors": [owner], "limit": 1})))
        .collect()
}

// ---------------------------------------------------------------------------
// Commands

/// A lifecycle command, published directly (never through the outbox).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Join,
    Leave,
    Archive,
    Unarchive,
    /// Streams and forums, by an owner (9008).
    Delete,
    /// DMs only (41012).
    Hide,
}

impl Action {
    /// Whether the permissions allow this action. Join starts from a
    /// non-member preview and is checked by [`join_ready`] instead.
    pub fn allowed(self, permissions: &LifecyclePermissions) -> bool {
        match self {
            Self::Join => false,
            Self::Leave => permissions.can_leave,
            Self::Archive => permissions.can_archive,
            Self::Unarchive => permissions.can_unarchive,
            Self::Delete => permissions.can_delete,
            Self::Hide => permissions.can_hide,
        }
    }
}

/// `lifecycleTemplate`: content `""`, tags `[["h", id]]` plus `archived`
/// for archive and unarchive.
pub fn template(action: Action, channel: &str, now: u64) -> Result<EventTemplate, LifecycleError> {
    let channel = channel_id(channel)?;
    let kind = match action {
        Action::Join => kinds::JOIN_CHANNEL,
        Action::Leave => kinds::LEAVE_CHANNEL,
        Action::Archive | Action::Unarchive => kinds::EDIT_CHANNEL,
        Action::Delete => kinds::DELETE_CHANNEL,
        Action::Hide => kinds::HIDE_DM,
    };
    let mut tags = vec![vec!["h".to_owned(), channel.to_owned()]];
    match action {
        Action::Archive => tags.push(vec!["archived".into(), "true".into()]),
        Action::Unarchive => tags.push(vec!["archived".into(), "false".into()]),
        _ => {}
    }
    Ok(EventTemplate {
        kind,
        created_at: now,
        tags,
        content: String::new(),
    })
}

/// The confirmation read for an action (§5m.2): the viewer's roster for
/// join and leave, the metadata for archive and delete, the visibility
/// list for hide.
pub fn confirm_filter(action: Action, relay_author: &str, viewer: &str, channel: &str) -> Filter {
    match action {
        Action::Join | Action::Leave => filter(json!({
            "kinds": [kinds::CHANNEL_MEMBERS],
            "consistency": "strong",
            "authors": [relay_author],
            "#d": [channel],
            "#p": [viewer],
            "limit": 1,
        })),
        Action::Archive | Action::Unarchive | Action::Delete => filter(json!({
            "kinds": [kinds::CHANNEL_METADATA],
            "consistency": "strong",
            "authors": [relay_author],
            "#d": [channel],
            "limit": 1,
        })),
        Action::Hide => visibility_filter(relay_author, viewer, true),
    }
}

/// Whether a confirmation read shows the action took effect.
pub fn confirmed(
    action: Action,
    events: &[Verified],
    relay_author: &str,
    viewer: &str,
    channel: &str,
) -> bool {
    let from_relay =
        |event: &&Verified| event.pubkey_hex() == relay_author && event.tag("d") == Some(channel);
    let newest = |kind: u16| {
        events
            .iter()
            .filter(|event| event.kind_u16() == kind)
            .filter(from_relay)
            .reduce(|a, b| if is_newer(b, a) { b } else { a })
    };
    match action {
        Action::Join => newest(kinds::CHANNEL_MEMBERS).is_some_and(|r| r.has_tag("p", viewer)),
        Action::Leave => !newest(kinds::CHANNEL_MEMBERS).is_some_and(|r| r.has_tag("p", viewer)),
        Action::Archive => {
            newest(kinds::CHANNEL_METADATA).is_some_and(|m| m.tag("archived") == Some("true"))
        }
        Action::Unarchive => newest(kinds::CHANNEL_METADATA)
            .is_some_and(|m| matches!(m.tag("archived"), None | Some("false"))),
        // The relay soft-deletes the channel's discovery events.
        Action::Delete => newest(kinds::CHANNEL_METADATA).is_none(),
        Action::Hide => hidden_dms(events, relay_author, viewer)
            .is_some_and(|ids| ids.iter().any(|id| id == channel)),
    }
}

/// Join preconditions (`channel-lifecycle.ts:302-320`): the fresh 39000
/// must be open public metadata and not archived. `Ok(true)` when the
/// viewer is already a member, so nothing needs publishing.
pub fn join_ready(
    events: &[Verified],
    relay_author: &str,
    viewer: &str,
    channel: &str,
) -> Result<bool, LifecycleError> {
    // A current roster entry means there is nothing to send.
    if record(events, kinds::CHANNEL_MEMBERS, channel, relay_author)?
        .is_some_and(|roster| roster.has_tag("p", viewer))
    {
        return Ok(true);
    }
    let joinable =
        record(events, kinds::CHANNEL_METADATA, channel, relay_author)?.is_some_and(|metadata| {
            crate::discovery::open_metadata(metadata) && metadata.tag("archived") != Some("true")
        });
    if !joinable {
        return Err(error("Only active public channels can be joined."));
    }
    Ok(false)
}

/// Strong read of the 39000 and the viewer's 39002 for a join.
pub fn join_filters(relay_author: &str, viewer: &str, channel: &str) -> Vec<Filter> {
    vec![
        confirm_filter(Action::Archive, relay_author, viewer, channel),
        confirm_filter(Action::Join, relay_author, viewer, channel),
    ]
}

// ---------------------------------------------------------------------------
// Create (9007)

/// `canonical_channel_name`: no leading `#` or whitespace, no trailing
/// whitespace.
pub fn canonical_name(name: &str) -> String {
    name.trim_start_matches(|c: char| c == '#' || c.is_whitespace())
        .trim_end()
        .to_owned()
}

/// Inline field feedback for a draft as typed (`detailsDraftErrors`). The
/// command validators stay the write boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DraftErrors {
    pub name: Option<&'static str>,
    pub description: Option<&'static str>,
    pub lifetime: Option<&'static str>,
}

impl DraftErrors {
    pub fn any(&self) -> bool {
        self.name.is_some() || self.description.is_some() || self.lifetime.is_some()
    }
}

/// `detailsDraftErrors`. Pass the [`canonical_name`] of what was typed.
pub fn draft_errors(name: &str, description: &str, ttl: Option<u64>) -> DraftErrors {
    DraftErrors {
        name: validate_name(name).err().map(|_| NAME_ERROR),
        description: if description.chars().count() > MAX_ABOUT_CHARS {
            Some(ABOUT_LENGTH_ERROR)
        } else if description.contains("Buzz session (") {
            Some(ABOUT_SESSION_ERROR)
        } else {
            None
        },
        lifetime: validate_ttl(ttl).err().map(|_| TTL_ERROR),
    }
}

fn validate_name(name: &str) -> Result<(), LifecycleError> {
    if name.is_empty() || name != canonical_name(name) || name.chars().count() > MAX_NAME_CHARS {
        return Err(error(NAME_ERROR));
    }
    Ok(())
}

fn validate_about(about: &str) -> Result<(), LifecycleError> {
    if about.chars().count() > MAX_ABOUT_CHARS {
        return Err(error(ABOUT_LENGTH_ERROR));
    }
    if about.contains("Buzz session (") {
        return Err(error(ABOUT_SESSION_ERROR));
    }
    Ok(())
}

/// A TTL must be a positive i32; `None` is ongoing.
fn validate_ttl(ttl: Option<u64>) -> Result<(), LifecycleError> {
    if ttl.is_some_and(|ttl| ttl == 0 || ttl > MAX_TTL) {
        return Err(error(TTL_ERROR));
    }
    Ok(())
}

/// What the create dialog submits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewChannel {
    pub name: String,
    pub description: String,
    pub private: bool,
    /// Seconds without activity before the relay cleans the channel up;
    /// `None` is ongoing.
    pub ttl: Option<u64>,
    /// NIP-CL labels to create the channel with.
    pub labels: Vec<String>,
}

/// A 9007 stream for the outbox, with the client-chosen `id`. Tag order is
/// enforced by the relay: `h, name, visibility, channel_type[, about][,
/// ttl]`, then any label `t` tags.
pub fn create(channel: &NewChannel, id: &str) -> Result<Draft, LifecycleError> {
    let id = channel_id(id)?;
    let name = canonical_name(&channel.name);
    validate_name(&name)?;
    let about = channel.description.trim();
    validate_about(about)?;
    validate_ttl(channel.ttl)?;
    let labels = crate::channel_labels::label_tags(&channel.labels).map_err(error)?;
    let mut tags = vec![
        vec!["h".to_owned(), id.to_owned()],
        vec!["name".to_owned(), name],
        vec![
            "visibility".to_owned(),
            if channel.private { "private" } else { "open" }.to_owned(),
        ],
        vec!["channel_type".to_owned(), "stream".to_owned()],
    ];
    if !about.is_empty() {
        tags.push(vec!["about".to_owned(), about.to_owned()]);
    }
    if let Some(ttl) = channel.ttl {
        tags.push(vec!["ttl".to_owned(), ttl.to_string()]);
    }
    tags.extend(labels);
    Ok(Draft {
        kind: kinds::CREATE_CHANNEL,
        content: String::new(),
        tags,
    })
}

/// The strong creation receipt check after an uncertain delivery.
pub fn creation_receipt_filter(id: &EventId, viewer: &str) -> Filter {
    filter(json!({
        "kinds": [kinds::CREATE_CHANNEL],
        "ids": [id.to_hex()],
        "authors": [viewer],
        "limit": 1,
        "consistency": "strong",
    }))
}

/// The new channel's metadata and the viewer's roster entry.
pub fn created_filters(relay_author: &str, viewer: &str, channel: &str) -> Vec<Filter> {
    join_filters(relay_author, viewer, channel)
}

// ---------------------------------------------------------------------------
// Details (9002)

/// The editable details of a channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailsDraft {
    pub name: String,
    pub description: String,
    pub private: bool,
    /// `None` is permanent.
    pub ttl: Option<u64>,
    /// NIP-CL labels. `None` leaves the relay's set unchanged; empty
    /// clears it.
    pub labels: Option<Vec<String>>,
}

impl DetailsDraft {
    /// The labels change from `base`'s.
    fn relabels(&self, base: &DetailsDraft) -> bool {
        self.labels.as_ref().is_some_and(|labels| {
            !crate::channel_labels::same_labels(labels, base.labels.as_deref().unwrap_or_default())
        })
    }
}

/// A details save the session owns (`DetailsAttempt`). It outlives the
/// panel that started it, so reopening still shows an uncertain save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailsAttempt {
    pub draft: DetailsDraft,
    pub status: AttemptStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptStatus {
    Saving,
    /// Published, but the relay did not show it. Only a check clears it;
    /// nothing is resent.
    Unconfirmed,
}

/// An uncertain details save (`channel-details.ts`).
pub const UNCERTAIN_SAVE: &str =
    "The change may have been saved. Check its status before trying again.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelDetails {
    pub channel: String,
    /// Saving refuses when the channel's 39000 moved past this.
    pub version: EventId,
    pub draft: DetailsDraft,
    pub can_edit: bool,
}

/// `validateDetailsDraft`: a root session's description is its marker,
/// so only its name may change.
pub fn validate_details(draft: &DetailsDraft, base: &DetailsDraft) -> Result<(), LifecycleError> {
    let mut checked = draft.clone();
    if crate::discovery::session_metadata(Some(&base.description)).is_some_and(|p| p.is_none())
        && !draft.description.is_empty()
    {
        if draft.description != base.description
            || draft.relabels(base)
            || draft.private != base.private
            || draft.ttl != base.ttl
        {
            return Err(error("Only the name of a session can be changed here."));
        }
        checked.description.clear();
    }
    let errors = draft_errors(&checked.name, &checked.description, checked.ttl);
    if let Some(message) = errors.name.or(errors.description).or(errors.lifetime) {
        return Err(error(message));
    }
    match &draft.labels {
        Some(labels) => crate::channel_labels::label_tags(labels)
            .map(drop)
            .map_err(error),
        None => Ok(()),
    }
}

/// Saving these details changes the labels on a relay that would drop
/// them (`channelIdentityUnsupported`).
pub fn labels_unsupported(draft: &DetailsDraft, base: &DetailsDraft, supported: bool) -> bool {
    !supported && draft.relabels(base)
}

/// `detailsTemplate`: name and about always; visibility and ttl only when
/// they change (an unchanged ttl would reset the relay's cleanup). A
/// session keeping its marker sends its name alone.
pub fn edit_details(
    channel: &str,
    draft: &DetailsDraft,
    base: &DetailsDraft,
    now: u64,
) -> Result<EventTemplate, LifecycleError> {
    let channel = channel_id(channel)?;
    validate_details(draft, base)?;
    if crate::discovery::session_metadata(Some(&base.description)).is_some()
        && draft.description == base.description
    {
        return Ok(EventTemplate {
            kind: kinds::EDIT_CHANNEL,
            created_at: now,
            tags: vec![
                vec!["h".to_owned(), channel.to_owned()],
                vec!["name".to_owned(), draft.name.clone()],
            ],
            content: String::new(),
        });
    }
    let mut tags = vec![
        vec!["h".to_owned(), channel.to_owned()],
        vec!["name".to_owned(), draft.name.clone()],
        vec!["about".to_owned(), draft.description.clone()],
    ];
    if draft.private != base.private {
        tags.push(vec![
            "visibility".to_owned(),
            if draft.private { "private" } else { "open" }.to_owned(),
        ]);
    }
    if draft.ttl != base.ttl {
        tags.push(vec![
            "ttl".to_owned(),
            draft.ttl.map(|ttl| ttl.to_string()).unwrap_or_default(),
        ]);
    }
    // NIP-CL: `t` tags replace the set; one empty `t` clears it; none
    // keeps it.
    if let Some(labels) = draft.labels.as_ref().filter(|_| draft.relabels(base)) {
        if labels.is_empty() {
            tags.push(vec!["t".to_owned(), String::new()]);
        } else {
            tags.extend(crate::channel_labels::label_tags(labels).map_err(error)?);
        }
    }
    Ok(EventTemplate {
        kind: kinds::EDIT_CHANNEL,
        created_at: now,
        tags,
        content: String::new(),
    })
}

/// A re-read state shows the saved draft.
pub fn details_confirmed(state: &ChannelState, draft: &DetailsDraft) -> bool {
    state.name == draft.name
        && state.about == draft.description
        && state.visibility
            == Some(if draft.private {
                Visibility::Private
            } else {
                Visibility::Public
            })
        && state.ttl == draft.ttl
        && draft
            .labels
            .as_ref()
            .is_none_or(|labels| crate::channel_labels::same_labels(labels, &state.labels))
}

// ---------------------------------------------------------------------------
// DM visibility (30622, §5e.5)

/// The viewer's relay-signed list of hidden DMs. `strong` on confirmation.
pub fn visibility_filter(relay_author: &str, viewer: &str, strong: bool) -> Filter {
    let mut f = filter(json!({
        "kinds": [kinds::HIDDEN_DMS],
        "authors": [relay_author],
        "#d": [viewer],
        "#p": [viewer],
        "limit": 1,
    }));
    if strong {
        f.insert("consistency".into(), json!("strong"));
    }
    f
}

/// Hidden DM ids from the newest valid 30622, or `None` when no list
/// exists (nothing is hidden). Invalid lists are ignored.
pub fn hidden_dms(events: &[Verified], relay_author: &str, viewer: &str) -> Option<Vec<String>> {
    visibility_record(events, relay_author, viewer)
        .ok()
        .flatten()
        .map(|record| record.hidden)
}

/// The viewer's newest relay-signed DM visibility list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibilityRecord {
    pub id: EventId,
    pub created_at: u64,
    pub hidden: Vec<String>,
}

impl VisibilityRecord {
    /// `newer`: later `created_at`, a same-second tie to the lower id.
    pub fn newer_than(&self, other: &Self) -> bool {
        self.created_at > other.created_at
            || (self.created_at == other.created_at && self.id < other.id)
    }
}

/// `readVisibility`: the newest 30622, or `None` when there is none. A
/// list that names someone else, has content, or a malformed entry is an
/// error, never "nothing hidden".
pub fn visibility_record(
    events: &[Verified],
    relay_author: &str,
    viewer: &str,
) -> Result<Option<VisibilityRecord>, LifecycleError> {
    let Some(newest) = record(events, kinds::HIDDEN_DMS, viewer, relay_author)? else {
        return Ok(None);
    };
    let people: Vec<&[String]> = newest.tags_named("p").collect();
    if !newest.content.is_empty()
        || people.len() != 1
        || people[0].len() != 2
        || people[0][1] != viewer
    {
        return Err(error("Invalid DM visibility snapshot"));
    }
    let mut hidden = Vec::new();
    for tag in newest.tags_named("h") {
        if tag.len() != 2 || !is_uuid(&tag[1]) {
            return Err(error("Invalid hidden DM"));
        }
        if !hidden.contains(&tag[1]) {
            hidden.push(tag[1].clone());
        }
    }
    Ok(Some(VisibilityRecord {
        id: newest.id,
        created_at: newest.created_at_secs(),
        hidden,
    }))
}

#[cfg(test)]
mod tests;
