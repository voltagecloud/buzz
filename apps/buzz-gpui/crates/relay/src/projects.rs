//! Projects (`features/projects`): NIP-34 repositories (kind 30617),
//! NIP-MP projects (30621), their issues (1621) and pull requests (1618).
//!
//! - Entity links and page routes (`routes.ts`): `buzz://repo|project|pr|issue`.
//! - Discovery and validation (`destinations.ts`): the recent directory,
//!   an entity's detail with its repositories, channels, items and
//!   discussion, and a channel's project home for the resource picker.
//! - The shape of a repository Git read (`git.ts`), which buzz-desktop's
//!   `project_git` performs over the relay's Git HTTP.
//!
//! A valid address is never authorization: every read goes through the
//! viewer's verified session reads, and the relay decides what they see.

use crate::budget::ByteLru;
use crate::event::{Verified, is_newer, newest};
use crate::reader::{ReadError, ReadErrorKind};
use crate::socket::{Filter, filter};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

pub const REPOSITORY: u16 = 30617;
pub const PROJECT: u16 = 30621;
pub const PULL_REQUEST: u16 = 1618;
pub const PULL_REQUEST_UPDATE: u16 = 1619;
pub const ISSUE: u16 = 1621;
pub const STATUS_OPEN: u16 = 1630;
pub const STATUS_APPLIED: u16 = 1631;
pub const STATUS_CLOSED: u16 = 1632;
pub const STATUS_DRAFT: u16 = 1633;
const DELETION: u16 = 5;
const NOTE: u16 = 1;
const COMMENT: u16 = 1111;

/// Largest Git read response (`GIT_READ_BYTES`).
pub const GIT_READ_BYTES: usize = 4 * 1024 * 1024;
/// Discovery and item lists are finite (`limit: 100`).
const LIST_LIMIT: usize = 100;
/// Member repositories read at once.
const MEMBER_READS: usize = 6;
/// Deletion and discussion reads (`limit: 500`); a full one is unavailable.
const ACTIVITY_LIMIT: usize = 500;

// ── routes.ts ──────────────────────────────────────────────────────────────

/// An entity page's tab; Overview is the absence of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityTab {
    Files,
    Commits,
    Issues,
    Prs,
    Contributors,
    Channels,
}

impl EntityTab {
    /// `entityTabs`, in order.
    pub const ALL: [EntityTab; 6] = [
        Self::Files,
        Self::Commits,
        Self::Issues,
        Self::Prs,
        Self::Contributors,
        Self::Channels,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Commits => "commits",
            Self::Issues => "issues",
            Self::Prs => "prs",
            Self::Contributors => "contributors",
            Self::Channels => "channels",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tab| tab.as_str() == value)
    }

    /// Tabs that show repository content and fail without it.
    pub fn needs_git(self) -> bool {
        matches!(self, Self::Files | Self::Commits | Self::Contributors)
    }
}

/// What a route names (`EntityRoute.type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RouteKind {
    Repo,
    Project,
    Pr,
    Issue,
}

impl RouteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Project => "project",
            Self::Pr => "pr",
            Self::Issue => "issue",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Self::Repo, Self::Project, Self::Pr, Self::Issue]
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }

    /// The query keys of this kind's link grammar.
    fn keys(self) -> &'static [&'static str] {
        match self {
            Self::Repo => &["owner", "d", "tab", "commit"],
            Self::Project => &["owner", "d", "tab"],
            Self::Pr | Self::Issue => &["owner", "d", "id"],
        }
    }
}

/// A Projects page route (`EntityRoute`). Only the parsers build one, so
/// `tab` and `commit` appear only on repositories and projects (`commit`
/// only on a repository's commits tab) and `id` only on items.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntityRoute {
    pub kind: RouteKind,
    /// Lowercase hex.
    pub owner: String,
    pub dtag: String,
    pub tab: Option<EntityTab>,
    /// Lowercase hex.
    pub commit: Option<String>,
    /// The issue or pull request event, lowercase hex.
    pub id: Option<String>,
}

impl EntityRoute {
    /// A repository or project's Overview. `None` when the coordinate
    /// cannot be linked.
    pub fn entity(kind: EntityKind, owner: &str, dtag: &str) -> Option<Self> {
        let kind = match kind {
            EntityKind::Repo => RouteKind::Repo,
            EntityKind::Project => RouteKind::Project,
        };
        (entity_hex(owner) && entity_dtag(dtag)).then(|| Self {
            kind,
            owner: owner.to_ascii_lowercase(),
            dtag: dtag.to_string(),
            tab: None,
            commit: None,
            id: None,
        })
    }

    /// An issue or pull request in repository `owner`/`dtag`.
    pub fn item(kind: ItemKind, owner: &str, dtag: &str, id: &str) -> Option<Self> {
        (entity_hex(owner) && entity_dtag(dtag) && entity_hex(id)).then(|| Self {
            kind: match kind {
                ItemKind::Pr => RouteKind::Pr,
                ItemKind::Issue => RouteKind::Issue,
            },
            owner: owner.to_ascii_lowercase(),
            dtag: dtag.to_string(),
            tab: None,
            commit: None,
            id: Some(id.to_ascii_lowercase()),
        })
    }

    /// This repository or project at `tab` (Overview for `None`); items
    /// are unchanged.
    pub fn with_tab(mut self, tab: Option<EntityTab>) -> Self {
        if self.id.is_none() {
            self.tab = tab;
            self.commit = None;
        }
        self
    }

    /// A repository's commit on its Commits tab.
    pub fn with_commit(mut self, commit: &str) -> Self {
        if self.kind == RouteKind::Repo && git_hash(commit) {
            self.tab = Some(EntityTab::Commits);
            self.commit = Some(commit.to_ascii_lowercase());
        }
        self
    }

    /// The repository or project the route belongs to, at Overview.
    pub fn base(&self) -> Self {
        Self {
            kind: match self.kind {
                RouteKind::Pr | RouteKind::Issue => RouteKind::Repo,
                kind => kind,
            },
            owner: self.owner.clone(),
            dtag: self.dtag.clone(),
            tab: None,
            commit: None,
            id: None,
        }
    }

    /// `entityHref`: the canonical link.
    pub fn href(&self) -> String {
        let mut url = format!("buzz://{}?", self.kind.as_str());
        if let Some(id) = &self.id {
            url.push_str(&format!("id={id}&"));
        }
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("owner", &self.owner);
        query.append_pair("d", &self.dtag);
        if let Some(tab) = self.tab {
            query.append_pair("tab", tab.as_str());
        }
        if let Some(commit) = &self.commit {
            query.append_pair("commit", commit);
        }
        url.push_str(&query.finish());
        url
    }

    /// The route's params as the page records them (`entityTarget`).
    pub fn params(&self) -> Value {
        let mut params = serde_json::Map::new();
        params.insert("type".into(), json!(self.kind.as_str()));
        params.insert("owner".into(), json!(self.owner));
        params.insert("dtag".into(), json!(self.dtag));
        if let Some(id) = &self.id {
            params.insert("id".into(), json!(id));
        }
        if let Some(tab) = self.tab {
            params.insert("tab".into(), json!(tab.as_str()));
        }
        if let Some(commit) = &self.commit {
            params.insert("commit".into(), json!(commit));
        }
        Value::Object(params)
    }

    /// `parseEntityLink`: the canonical Buzz entity forms shared by OS
    /// ingress, message links and page routes. Query parameters outside the
    /// type's grammar are ignored; a repeated known key is ambiguous and
    /// rejects.
    pub fn parse_link(href: &str) -> Option<Self> {
        let url = url::Url::parse(href).ok()?;
        let kind = RouteKind::parse(url.host_str()?)?;
        if url.scheme() != "buzz"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return None;
        }
        let all = |key: &str| -> Vec<String> {
            url.query_pairs()
                .filter(|(name, _)| name == key)
                .map(|(_, value)| value.into_owned())
                .collect()
        };
        if kind.keys().iter().any(|key| all(key).len() > 1) {
            return None;
        }
        let one = |key: &str| all(key).into_iter().next();
        let owner = one("owner").unwrap_or_default();
        let dtag = one("d").unwrap_or_default();
        if !entity_hex(&owner) || !entity_dtag(&dtag) {
            return None;
        }
        let owner = owner.to_ascii_lowercase();
        if matches!(kind, RouteKind::Pr | RouteKind::Issue) {
            let id = one("id").unwrap_or_default();
            return entity_hex(&id).then(|| Self {
                kind,
                owner,
                dtag,
                tab: None,
                commit: None,
                id: Some(id.to_ascii_lowercase()),
            });
        }
        let tab = one("tab");
        // `commit` belongs to the repository grammar only; on a project it
        // is an unknown key.
        let commit = (kind == RouteKind::Repo).then(|| one("commit")).flatten();
        let tab = match tab {
            Some(tab) => Some(EntityTab::parse(&tab)?),
            None => None,
        };
        if let Some(commit) = &commit
            && (tab != Some(EntityTab::Commits) || !git_hash(commit))
        {
            return None;
        }
        Some(Self {
            kind,
            owner,
            dtag,
            tab,
            commit: commit.map(|c| c.to_ascii_lowercase()),
            id: None,
        })
    }

    /// `parseEntityRoute`: page route params, an object of strings with
    /// exactly the type's keys, validated as the equivalent link.
    pub fn from_params(params: &Value) -> Option<Self> {
        let route = params.as_object()?;
        let kind = RouteKind::parse(route.get("type")?.as_str()?)?;
        let allowed: Vec<&str> = std::iter::once("type")
            .chain(
                kind.keys()
                    .iter()
                    .map(|key| if *key == "d" { "dtag" } else { key }),
            )
            .collect();
        if route
            .iter()
            .any(|(key, value)| !allowed.contains(&key.as_str()) || !value.is_string())
        {
            return None;
        }
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in route {
            if key != "type" {
                let key = if key == "dtag" { "d" } else { key.as_str() };
                query.append_pair(key, value.as_str()?);
            }
        }
        Self::parse_link(&format!("buzz://{}?{}", kind.as_str(), query.finish()))
    }
}

/// `entityHex`: 64 hex digits in either case.
pub fn entity_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `gitHash`: a SHA-1 or SHA-256 object name in either case.
pub fn git_hash(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `entityDtag`: an identifier a Buzz link and the relay's Git routes
/// accept, `[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}` without `..`.
pub fn entity_dtag(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_alphanumeric() || matches!(bytes[0], b'_' | b'-'))
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !value.contains("..")
}

// ── destinations.ts ────────────────────────────────────────────────────────

/// Why a destination could not open (`OpenFailure`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityFailure {
    NotFound,
    Denied,
    Unavailable,
    InvalidTarget,
}

impl From<ReadError> for EntityFailure {
    /// `entityFailure`: a 404 is not-found, a denial is denied, anything
    /// else is unavailable.
    fn from(error: ReadError) -> Self {
        if error.status == Some(404) {
            Self::NotFound
        } else if error.kind == ReadErrorKind::Denied {
            Self::Denied
        } else {
            Self::Unavailable
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Repo,
    Project,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Pr,
    Issue,
}

impl ItemKind {
    pub fn event_kind(self) -> u16 {
        match self {
            Self::Pr => PULL_REQUEST,
            Self::Issue => ISSUE,
        }
    }
}

/// A repository or project announcement.
#[derive(Clone, Debug)]
pub struct Entity {
    pub kind: EntityKind,
    pub owner: String,
    pub dtag: String,
    /// `<kind>:<owner>:<d>`.
    pub address: String,
    pub name: String,
    pub description: String,
    pub event: Verified,
}

impl Entity {
    /// The route to this entity's Overview, or `None` when its identifier
    /// cannot be opened with a Buzz link.
    pub fn route(&self) -> Option<EntityRoute> {
        EntityRoute::entity(self.kind, &self.owner, &self.dtag)
    }
}

/// An entity with what its route needs.
#[derive(Clone, Debug)]
pub struct EntityDetail {
    pub entity: Entity,
    pub repositories: Vec<Entity>,
    /// Project members that are deleted, missing or hidden here.
    pub unavailable_repositories: Vec<String>,
    /// Linked and related channel ids.
    pub channels: Vec<String>,
    /// The issue or pull request the route names.
    pub item: Option<Verified>,
    pub status: Option<&'static str>,
    /// A pull request's tip commit, lowercase.
    pub commit: Option<String>,
    /// Comments on the item, oldest first.
    pub activity: Vec<Verified>,
    /// The Issues or Pull requests tab's items.
    pub items: Vec<Verified>,
    /// The item list read reached its limit; items may be missing.
    pub truncated: bool,
}

/// A channel's project (`home`).
#[derive(Clone, Debug)]
pub enum Home {
    None,
    Ambiguous,
    Project(Entity),
}

/// `values`: the non-empty second elements of tags named `name`.
pub fn values<'a>(event: &'a Verified, name: &str) -> Vec<&'a str> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().is_some_and(|n| n == name))
        .filter_map(|tag| tag.get(1).map(String::as_str))
        .filter(|value| !value.is_empty())
        .collect()
}

/// `value`: the first of [`values`].
pub fn value<'a>(event: &'a Verified, name: &str) -> Option<&'a str> {
    values(event, name).into_iter().next()
}

/// The second element of the only tag named `name`.
fn single_value<'a>(event: &'a Verified, name: &str) -> Option<&'a str> {
    let mut tags = event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|tag| tag.first().is_some_and(|n| n == name));
    let first = tags.next()?;
    tags.next()
        .is_none()
        .then(|| first.get(1).map(String::as_str))?
}

/// An issue or pull request's title: its `subject`, else its first line.
pub fn item_title(event: &Verified) -> String {
    value(event, "subject")
        .map(str::to_string)
        .unwrap_or_else(|| event.content.split('\n').next().unwrap_or("").to_string())
}

fn metadata_dtag(dtag: &str) -> bool {
    !dtag.is_empty() && dtag.len() <= 1024
}

/// `repositoryCoordinate`: `30617:<lowercase hex>:<d>`.
fn repository_coordinate(address: &str) -> Option<(String, String)> {
    let mut parts = address.splitn(3, ':');
    let (kind, owner, dtag) = (parts.next()?, parts.next().unwrap_or(""), parts.next());
    let dtag = dtag.unwrap_or("");
    (kind == "30617"
        && owner.len() == 64
        && owner
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && metadata_dtag(dtag))
    .then(|| (owner.to_string(), dtag.to_string()))
}

/// `entityFromEvent`: a repository or project announcement with exactly
/// one `d`. A project's member list must be unique and well formed, and
/// its metadata tags single.
pub fn entity_from_event(event: &Verified) -> Option<Entity> {
    let kind = event.kind.as_u16();
    let ds = values(event, "d");
    if !matches!(kind, REPOSITORY | PROJECT) || ds.len() != 1 || !metadata_dtag(ds[0]) {
        return None;
    }
    let dtag = ds[0].to_string();
    if kind == PROJECT {
        let members: Vec<&[String]> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice())
            .filter(|tag| tag.first().is_some_and(|n| n == "a"))
            .collect();
        let unique: HashSet<Option<&String>> = members.iter().map(|tag| tag.get(1)).collect();
        if members.len() > 64
            || unique.len() != members.len()
            || members.iter().any(|tag| {
                tag.len() < 2
                    || tag.len() > 3
                    || repository_coordinate(tag.get(1).map_or("", String::as_str)).is_none()
            })
            || ["name", "description", "buzz-channel", "buzz-visibility"]
                .iter()
                .any(|name| values(event, name).len() > 1)
        {
            return None;
        }
    }
    let owner = event.pubkey.to_hex();
    Some(Entity {
        kind: if kind == REPOSITORY {
            EntityKind::Repo
        } else {
            EntityKind::Project
        },
        address: format!("{kind}:{owner}:{dtag}"),
        name: value(event, "name").unwrap_or(&dtag).to_string(),
        description: value(event, "description")
            .map(str::to_string)
            .unwrap_or_else(|| {
                if kind == PROJECT {
                    String::new()
                } else {
                    event.content.clone()
                }
            }),
        owner,
        dtag,
        event: event.clone(),
    })
}

/// `authoritativeProjects` (buzz-acp's `pick_authoritative_project_home`).
/// A project's own `buzz-channel` is presentation only. It is
/// authoritative when an `a` member is a listed repository whose first
/// `buzz-channel` is this channel and whose owner or a maintainer signed
/// the project. Callers fail closed on more than one match.
pub fn authoritative_projects<'a>(
    projects: &'a [Verified],
    repositories: &[Verified],
    channel: &str,
) -> Vec<&'a Verified> {
    let listed = |event: &Verified| {
        !values(event, "buzz-visibility").contains(&"unlisted")
            && value(event, "d").is_some_and(|d| !d.trim().is_empty())
    };
    let repos: HashMap<String, Vec<String>> = repositories
        .iter()
        .filter(|event| {
            event.kind.as_u16() == REPOSITORY
                && listed(event)
                && value(event, "buzz-channel") == Some(channel)
        })
        .map(|event| {
            let maintainers = event
                .tags
                .iter()
                .map(|tag| tag.as_slice())
                .filter(|tag| tag.first().is_some_and(|n| n == "maintainers"))
                .flat_map(|tag| tag[1..].iter().map(|key| key.to_ascii_lowercase()))
                .collect();
            (
                format!(
                    "{}:{}",
                    event.pubkey.to_hex(),
                    value(event, "d").unwrap_or("").trim()
                ),
                maintainers,
            )
        })
        .collect();
    projects
        .iter()
        .filter(|event| {
            if event.kind.as_u16() != PROJECT
                || !listed(event)
                || !values(event, "buzz-channel").contains(&channel)
            {
                return false;
            }
            let signer = event.pubkey.to_hex();
            values(event, "a").iter().any(|address| {
                let mut parts = address.splitn(3, ':');
                let kind = parts.next().unwrap_or("");
                let owner = parts.next().unwrap_or("").trim().to_ascii_lowercase();
                let rest = parts.next().unwrap_or("").trim();
                let key = format!("{owner}:{rest}");
                match repos.get(&key) {
                    Some(maintainers) if kind == "30617" => {
                        owner == signer || maintainers.contains(&signer)
                    }
                    _ => false,
                }
            })
        })
        .collect()
}

/// Verified, principal-bound relay reads. The session supplies them; no
/// feature here can pick an origin or a signer.
pub trait ProjectReads: Send + Sync {
    fn read(
        &self,
        filters: Vec<Filter>,
    ) -> impl Future<Output = Result<Vec<Verified>, ReadError>> + Send;
}

/// Channel homes a session keeps.
const HOMES: usize = 64;
/// Their byte budget.
const HOMES_BYTES: usize = 1024 * 1024;

/// The last completed project answer for each channel, in session memory
/// only, so a recreated picker keeps its button steady while it checks
/// again. An older check cannot replace a newer completed one, and none
/// can restore an answer cleared after it started.
#[derive(Clone)]
pub struct Homes(Arc<Mutex<HomesState>>);

struct HomesState {
    cache: ByteLru<Home>,
    /// Checks by channel. A cancelled newer check must not stop a
    /// surviving older one from keeping its result.
    attempts: HashMap<String, Attempts>,
    epoch: u64,
}

#[derive(Default)]
struct Attempts {
    next: u64,
    completed: u64,
    pending: u32,
}

impl Default for Homes {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HomesState {
            cache: ByteLru::new(HOMES, HOMES_BYTES),
            attempts: HashMap::new(),
            epoch: 0,
        })))
    }
}

impl Homes {
    fn state(&self) -> std::sync::MutexGuard<'_, HomesState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The last completed answer for `channel`, if any.
    pub fn peek(&self, channel: &str) -> Option<Home> {
        self.state().cache.peek(channel).cloned()
    }

    /// Forgets every answer; checks in flight keep none of theirs.
    pub fn clear(&self) {
        let mut state = self.state();
        state.epoch += 1;
        state.attempts.clear();
        state.cache.clear();
    }

    fn begin(&self, channel: &str) -> HomeAttempt {
        let mut state = self.state();
        let epoch = state.epoch;
        let attempts = state.attempts.entry(channel.to_string()).or_default();
        attempts.next += 1;
        attempts.pending += 1;
        HomeAttempt {
            homes: self.clone(),
            channel: channel.to_string(),
            attempt: attempts.next,
            epoch,
        }
    }
}

/// One check of a channel's home. Dropping it unfinished is a
/// cancellation: the cache keeps what it had.
struct HomeAttempt {
    homes: Homes,
    channel: String,
    attempt: u64,
    epoch: u64,
}

impl HomeAttempt {
    /// Keeps a completed answer, or forgets the channel's after a failure,
    /// unless a newer check already completed. A check that outlived a
    /// clear fails.
    fn finish(self, result: Result<Home, EntityFailure>) -> Result<Home, EntityFailure> {
        let mut state = self.homes.state();
        if state.epoch != self.epoch {
            return Err(EntityFailure::Unavailable);
        }
        let newest = state
            .attempts
            .get_mut(&self.channel)
            .filter(|attempts| self.attempt > attempts.completed)
            .map(|attempts| attempts.completed = self.attempt)
            .is_some();
        if newest {
            match &result {
                Ok(home) => {
                    state
                        .cache
                        .set(self.channel.clone(), home.clone(), home.byte_size());
                }
                Err(_) => {
                    state.cache.remove(&self.channel);
                }
            }
        }
        // Dropping `self` locks again.
        drop(state);
        result
    }
}

impl Drop for HomeAttempt {
    fn drop(&mut self) {
        let mut state = self.homes.state();
        if state.epoch != self.epoch {
            return;
        }
        if let Some(attempts) = state.attempts.get_mut(&self.channel) {
            attempts.pending -= 1;
            if attempts.pending == 0 {
                state.attempts.remove(&self.channel);
            }
        }
    }
}

impl Home {
    /// About its JSON size, for the cache's byte budget.
    fn byte_size(&self) -> usize {
        match self {
            Self::None | Self::Ambiguous => 24,
            Self::Project(project) => {
                64 + project.owner.len()
                    + project.dtag.len()
                    + project.address.len()
                    + project.name.len()
                    + project.description.len()
                    + project.event.byte_size()
            }
        }
    }
}

/// `projectDestinations`.
pub struct Destinations<R> {
    read: R,
    homes: Homes,
}

impl<R: ProjectReads> Destinations<R> {
    pub fn new(read: R) -> Self {
        Self::with_homes(read, Homes::default())
    }

    /// Reads that keep channel homes in `homes`, the session's cache.
    pub fn with_homes(read: R, homes: Homes) -> Self {
        Self { read, homes }
    }

    /// The last completed answer for `channel` (`peekHome`).
    pub fn peek_home(&self, channel: &str) -> Option<Home> {
        self.homes.peek(channel)
    }

    async fn read(&self, filters: Vec<Value>) -> Result<Vec<Verified>, EntityFailure> {
        self.read
            .read(filters.into_iter().map(filter).collect())
            .await
            .map_err(|error| {
                tracing::debug!(%error, status = ?error.status, "projects read failed");
                EntityFailure::from(error)
            })
    }

    /// Drops entities their owner deleted, by id or by address at or after
    /// the announcement. A full deletion read cannot prove anything.
    async fn surviving(&self, entities: Vec<Entity>) -> Result<Vec<Entity>, EntityFailure> {
        if entities.is_empty() {
            return Ok(entities);
        }
        let mut authors: Vec<&str> = Vec::new();
        for entity in &entities {
            if !authors.contains(&entity.owner.as_str()) {
                authors.push(&entity.owner);
            }
        }
        let addresses: Vec<&str> = entities.iter().map(|e| e.address.as_str()).collect();
        let ids: Vec<String> = entities.iter().map(|e| e.event.id.to_hex()).collect();
        let events = self
            .read(vec![
                json!({"kinds": [DELETION], "authors": authors, "#a": addresses, "limit": ACTIVITY_LIMIT}),
                json!({"kinds": [DELETION], "authors": authors, "#e": ids, "limit": ACTIVITY_LIMIT}),
            ])
            .await?;
        if events.len() >= ACTIVITY_LIMIT {
            return Err(EntityFailure::Unavailable);
        }
        Ok(entities
            .into_iter()
            .filter(|entity| {
                let id = entity.event.id.to_hex();
                !events.iter().any(|e| {
                    e.kind.as_u16() == DELETION
                        && e.pubkey.to_hex() == entity.owner
                        && (values(e, "e").contains(&id.as_str())
                            || (values(e, "a").contains(&entity.address.as_str())
                                && e.created_at >= entity.event.created_at))
                })
            })
            .collect())
    }

    /// The newest surviving announcement at an exact coordinate.
    async fn coordinate(
        &self,
        kind: EntityKind,
        owner: &str,
        dtag: &str,
    ) -> Result<Entity, EntityFailure> {
        let event_kind = match kind {
            EntityKind::Repo => REPOSITORY,
            EntityKind::Project => PROJECT,
        };
        let events = self
            .read(vec![json!({
                "kinds": [event_kind], "authors": [owner], "#d": [dtag], "limit": 1
            })])
            .await?;
        let head = newest(events.iter().filter(|e| {
            e.kind.as_u16() == event_kind
                && e.pubkey.to_hex() == owner
                && values(e, "d").len() == 1
                && value(e, "d") == Some(dtag)
        }));
        let entity = head
            .and_then(entity_from_event)
            .ok_or(EntityFailure::NotFound)?;
        self.surviving(vec![entity])
            .await?
            .pop()
            .ok_or(EntityFailure::NotFound)
    }

    /// The channel's one project. Only a complete read is evidence of
    /// "none"; a capped read is unavailable. The answer is kept for
    /// [`Self::peek_home`]; a failure forgets the kept one.
    pub async fn home(&self, channel: &str) -> Result<Home, EntityFailure> {
        let attempt = self.homes.begin(channel);
        let result = self.read_home(channel).await;
        attempt.finish(result)
    }

    async fn read_home(&self, channel: &str) -> Result<Home, EntityFailure> {
        let events = self
            .read(
                [PROJECT, REPOSITORY]
                    .into_iter()
                    .map(|kind| json!({"kinds": [kind], "#buzz-channel": [channel], "limit": LIST_LIMIT}))
                    .collect(),
            )
            .await?;
        if [PROJECT, REPOSITORY]
            .into_iter()
            .any(|kind| events.iter().filter(|e| e.kind.as_u16() == kind).count() >= LIST_LIMIT)
        {
            return Err(EntityFailure::Unavailable);
        }
        let mut heads: Vec<(String, Verified)> = Vec::new();
        for event in &events {
            let address = format!(
                "{}:{}:{}",
                event.kind.as_u16(),
                event.pubkey.to_hex(),
                value(event, "d").unwrap_or("")
            );
            match heads.iter_mut().find(|(a, _)| *a == address) {
                Some((_, head)) => {
                    if is_newer(event, head) {
                        *head = event.clone();
                    }
                }
                None => heads.push((address, event.clone())),
            }
        }
        let heads: Vec<Verified> = heads.into_iter().map(|(_, e)| e).collect();
        let matches: Vec<Option<Entity>> = authoritative_projects(&heads, &heads, channel)
            .into_iter()
            .map(entity_from_event)
            .collect();
        // ACP would still name it; Projects cannot open it, so fail visibly.
        let Some(matches) = matches.into_iter().collect::<Option<Vec<Entity>>>() else {
            return Err(EntityFailure::Unavailable);
        };
        // Stricter than ACP: an owner-deleted project is not a home here.
        let mut surviving = self.surviving(matches).await?;
        Ok(match surviving.len() {
            0 => Home::None,
            1 => Home::Project(surviving.remove(0)),
            _ => Home::Ambiguous,
        })
    }

    /// The recent directory: up to 100 announcements, newest per address,
    /// without unlisted or deleted ones. Discovery is deliberately finite;
    /// direct links resolve their coordinate independently.
    pub async fn list(&self) -> Result<Vec<Entity>, EntityFailure> {
        let events = self
            .read(vec![
                json!({"kinds": [REPOSITORY, PROJECT], "limit": LIST_LIMIT}),
            ])
            .await?;
        let mut heads: Vec<Entity> = Vec::new();
        for event in &events {
            let Some(entity) = entity_from_event(event) else {
                continue;
            };
            match heads.iter_mut().find(|h| h.address == entity.address) {
                Some(head) => {
                    if is_newer(event, &head.event) {
                        *head = entity;
                    }
                }
                None => heads.push(entity),
            }
        }
        self.surviving(
            heads
                .into_iter()
                .filter(|entity| value(&entity.event, "buzz-visibility") != Some("unlisted"))
                .collect(),
        )
        .await
    }

    /// Everything `route` shows: the entity, a project's repositories, the
    /// linked channels, and the item with its status and discussion or the
    /// tab's item list.
    pub async fn load(&self, route: &EntityRoute) -> Result<EntityDetail, EntityFailure> {
        let kind = match route.kind {
            RouteKind::Project => EntityKind::Project,
            _ => EntityKind::Repo,
        };
        let entity = self.coordinate(kind, &route.owner, &route.dtag).await?;
        let mut repositories = Vec::new();
        let mut unavailable_repositories = Vec::new();
        if entity.kind == EntityKind::Repo {
            repositories.push(entity.clone());
        } else {
            let mut members: Vec<&str> = values(&entity.event, "a");
            members.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let coordinates = members
                .iter()
                .map(|address| repository_coordinate(address).ok_or(EntityFailure::Unavailable))
                .collect::<Result<Vec<_>, _>>()?;
            // A few at a time, in order: the relay lane runs six at once.
            let mut reads = futures::stream::iter(coordinates)
                .map(|(owner, dtag)| async move {
                    self.coordinate(EntityKind::Repo, &owner, &dtag).await
                })
                .buffered(MEMBER_READS);
            for address in members {
                let read = reads.next().await.expect("one read per member");
                match read {
                    Ok(repository) => repositories.push(repository),
                    Err(EntityFailure::NotFound | EntityFailure::Denied) => {
                        unavailable_repositories.push(address.to_string())
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        let mut channels: Vec<String> = Vec::new();
        for e in std::iter::once(&entity).chain(&repositories) {
            for channel in values(&e.event, "buzz-channel")
                .into_iter()
                .chain(values(&e.event, "buzz-related-channel"))
            {
                if !channels.iter().any(|c| c == channel) {
                    channels.push(channel.to_string());
                }
            }
        }
        let mut result = EntityDetail {
            entity,
            repositories,
            unavailable_repositories,
            channels,
            item: None,
            status: None,
            commit: None,
            activity: Vec::new(),
            items: Vec::new(),
            truncated: false,
        };
        if let Some(id) = &route.id {
            self.load_item(route.kind, id, &mut result).await?;
        } else if let Some(tab @ (EntityTab::Prs | EntityTab::Issues)) = route.tab
            && !result.repositories.is_empty()
        {
            let kind = match tab {
                EntityTab::Prs => PULL_REQUEST,
                _ => ISSUE,
            };
            let addresses: Vec<&str> = result
                .repositories
                .iter()
                .map(|r| r.address.as_str())
                .collect();
            let events = self
                .read(vec![
                    json!({"kinds": [kind], "#a": addresses, "limit": LIST_LIMIT}),
                ])
                .await?;
            result.truncated = events.len() >= LIST_LIMIT;
            result.items = events
                .iter()
                .filter(|e| {
                    e.kind.as_u16() == kind && values(e, "a").iter().any(|a| addresses.contains(a))
                })
                .cloned()
                .collect();
        }
        Ok(result)
    }

    async fn load_item(
        &self,
        kind: RouteKind,
        id: &str,
        result: &mut EntityDetail,
    ) -> Result<(), EntityFailure> {
        let pr = kind == RouteKind::Pr;
        let event_kind = if pr { PULL_REQUEST } else { ISSUE };
        let entity = &result.entity;
        let events = self
            .read(vec![
                json!({"ids": [id], "kinds": [event_kind], "limit": 1}),
            ])
            .await?;
        let item = events
            .into_iter()
            .find(|e| {
                e.id.to_hex() == id
                    && e.kind.as_u16() == event_kind
                    && values(e, "a").contains(&entity.address.as_str())
            })
            .ok_or(EntityFailure::NotFound)?;
        let activity = self
            .read(vec![
                json!({
                    "kinds": [NOTE, COMMENT, PULL_REQUEST_UPDATE, STATUS_OPEN, STATUS_APPLIED, STATUS_CLOSED, STATUS_DRAFT, DELETION],
                    "#e": [id], "limit": ACTIVITY_LIMIT
                }),
                json!({
                    "kinds": [NOTE, COMMENT, PULL_REQUEST_UPDATE, STATUS_OPEN, STATUS_APPLIED, STATUS_CLOSED, STATUS_DRAFT],
                    "#E": [id], "limit": ACTIVITY_LIMIT
                }),
            ])
            .await?;
        if activity.len() >= ACTIVITY_LIMIT {
            return Err(EntityFailure::Unavailable);
        }
        let author = item.pubkey.to_hex();
        if activity.iter().any(|e| {
            e.kind.as_u16() == DELETION
                && e.pubkey.to_hex() == author
                && values(e, "e").contains(&id)
        }) {
            return Err(EntityFailure::NotFound);
        }
        let mut status_authors: HashSet<String> =
            HashSet::from([author.clone(), entity.owner.clone()]);
        status_authors.extend(
            entity
                .event
                .tags
                .iter()
                .map(|tag| tag.as_slice())
                .filter(|tag| tag.first().is_some_and(|n| n == "maintainers"))
                .flat_map(|tag| tag[1..].iter())
                .filter(|key| entity_hex(key))
                .cloned(),
        );
        let update = newest(activity.iter().filter(|e| {
            e.kind.as_u16() == PULL_REQUEST_UPDATE
                && single_value(e, "E") == Some(id)
                && single_value(e, "P") == Some(author.as_str())
                && single_value(e, "a") == Some(entity.address.as_str())
                && single_value(e, "c").is_some_and(git_hash)
                && e.tags.iter().any(|t| {
                    let t = t.as_slice();
                    t.first().is_some_and(|n| n == "clone")
                        && t[1..].iter().any(|url| !url.trim().is_empty())
                })
                && e.pubkey.to_hex() == author
        }));
        let commit = single_value(update.unwrap_or(&item), "c");
        if pr && let Some(commit) = commit.filter(|c| git_hash(c)) {
            result.commit = Some(commit.to_ascii_lowercase());
        }
        let status = newest(activity.iter().filter(|e| {
            let roots: Vec<&[String]> = e
                .tags
                .iter()
                .map(|t| t.as_slice())
                .filter(|t| {
                    t.first().is_some_and(|n| n == "e") && t.get(3).is_some_and(|m| m == "root")
                })
                .collect();
            (STATUS_OPEN..=STATUS_DRAFT).contains(&e.kind.as_u16())
                && roots.len() == 1
                && roots[0].get(1).is_some_and(|r| r == id)
                && e.tags
                    .iter()
                    .map(|t| t.as_slice())
                    .filter(|t| t.first().is_some_and(|n| n == "a"))
                    .all(|t| t.get(1).is_some_and(|a| *a == entity.address))
                && status_authors.contains(&e.pubkey.to_hex())
        }));
        result.status = Some(match status.map(|s| s.kind.as_u16()) {
            Some(STATUS_APPLIED) if pr => "Merged",
            Some(STATUS_APPLIED) => "Done",
            Some(STATUS_CLOSED) => "Closed",
            Some(STATUS_DRAFT) => "Draft",
            Some(_) => "Open",
            None if values(&item, "t")
                .iter()
                .any(|label| label.eq_ignore_ascii_case("draft")) =>
            {
                "Draft"
            }
            None => "Open",
        });
        let related = |e: &Verified| {
            e.tags.iter().any(|t| {
                let t = t.as_slice();
                t.first().is_some_and(|n| n == "e" || n == "E") && t.get(1).is_some_and(|v| v == id)
            })
        };
        let mut seen = HashSet::new();
        let mut comments: Vec<Verified> = activity
            .iter()
            .filter(|e| matches!(e.kind.as_u16(), NOTE | COMMENT) && related(e))
            .filter(|e| seen.insert(e.id))
            .cloned()
            .collect();
        comments.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.to_hex().cmp(&b.id.to_hex()))
        });
        result.activity = comments;
        result.item = Some(item);
        Ok(())
    }
}

// ── git.ts ─────────────────────────────────────────────────────────────────

/// One repository read: its owner and `d`, optionally a commit to show and
/// a file to read. buzz-desktop's `project_git` re-validates it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GitRead {
    pub owner: String,
    pub dtag: String,
    pub commit: Option<String>,
    pub path: Option<String>,
}

impl GitRead {
    /// `parseGitRead`: lowercase hex owner and commit, a linkable `d`, and
    /// a relative path of at most 4096 UTF-16 units without control
    /// characters or empty, `.` or `..` segments.
    pub fn parse(self) -> Option<Self> {
        let path_ok = self.path.as_deref().is_none_or(|path| {
            path.encode_utf16().count() <= 4096
                && !path.chars().any(|c| (c as u32) < 32 || c as u32 == 127)
                && path
                    .split('/')
                    .all(|p| !p.is_empty() && p != "." && p != "..")
        });
        (entity_hex(&self.owner)
            && entity_dtag(&self.dtag)
            && self.commit.as_deref().is_none_or(git_hash)
            && path_ok)
            .then(|| Self {
                owner: self.owner.to_ascii_lowercase(),
                commit: self.commit.map(|c| c.to_ascii_lowercase()),
                ..self
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct GitCommit {
    pub hash: String,
    pub author: String,
    pub date: u64,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct GitFile {
    pub path: String,
    pub hash: String,
    pub size: u64,
}

/// A file's text, `None` when binary or larger than 1 MiB.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct GitFileContent {
    pub path: String,
    pub content: Option<String>,
    pub size: u64,
}

/// What a Git read returns (`GitSnapshot`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct GitSnapshot {
    /// `None` for an empty repository.
    pub head: Option<String>,
    /// The latest commits, at most 100.
    pub commits: Vec<GitCommit>,
    pub files: Vec<GitFile>,
    pub readme: Option<String>,
    /// The file the read named.
    pub file: Option<GitFileContent>,
    /// `git show` of the commit the read named.
    pub diff: Option<String>,
}

const SAFE_INTEGER: u64 = (1 << 53) - 1;

/// `projectGitHost`'s response checks: a failed status is denied (401,
/// 403), not found (404) or unavailable; the body is bounded before
/// parsing and must match what the read asked for.
pub fn git_response(read: &GitRead, status: u16, body: &str) -> Result<GitSnapshot, EntityFailure> {
    if !(200..300).contains(&status) {
        return Err(match status {
            401 | 403 => EntityFailure::Denied,
            404 => EntityFailure::NotFound,
            _ => EntityFailure::Unavailable,
        });
    }
    if body.len() > GIT_READ_BYTES {
        return Err(EntityFailure::Unavailable);
    }
    let value: GitSnapshot = serde_json::from_str(body).map_err(|_| EntityFailure::Unavailable)?;
    let valid = value.head.as_deref().is_none_or(git_hash)
        && read
            .commit
            .as_ref()
            .is_none_or(|commit| value.head.as_ref() == Some(commit))
        && value.commits.len() <= 100
        && value
            .commits
            .iter()
            .all(|c| git_hash(&c.hash) && c.date <= SAFE_INTEGER)
        && value.files.len() <= 20_000
        && value
            .files
            .iter()
            .all(|f| git_hash(&f.hash) && f.size <= SAFE_INTEGER)
        && (value.head.is_some()
            || (value.commits.is_empty()
                && value.files.is_empty()
                && value.readme.is_none()
                && value.file.is_none()
                && value.diff.is_none()))
        && (read.commit.is_none() || value.diff.is_some())
        && read
            .path
            .as_ref()
            .is_none_or(|path| value.file.as_ref().is_some_and(|f| f.path == *path))
        && value.file.as_ref().is_none_or(|f| f.size <= SAFE_INTEGER);
    valid.then_some(value).ok_or(EntityFailure::Unavailable)
}

/// `communityGitRepository`: the canonical URL of a Buzz Git repository
/// hosted by this community (`origin` is its https origin), or `None` for
/// any other URL.
pub fn community_git_repository(origin: &str, input: &str) -> Option<String> {
    let url = url::Url::parse(input.trim()).ok()?;
    let origin = url::Url::parse(origin).ok()?;
    let mut segments = url.path().split('/').skip(1);
    let (git, owner, name) = (
        segments.next().unwrap_or(""),
        segments.next().unwrap_or(""),
        segments.next().unwrap_or(""),
    );
    (url.scheme() == "https"
        && url.origin() == origin.origin()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && git == "git"
        && segments.next().is_none()
        && owner.len() == 64
        && owner
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && entity_dtag(name.strip_suffix(".git").unwrap_or(name)))
    .then(|| url.to_string())
}

#[cfg(test)]
#[path = "projects_tests.rs"]
mod tests;
