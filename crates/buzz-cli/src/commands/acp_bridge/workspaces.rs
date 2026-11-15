//! Workspace routing for new thread roots and `session/list` placement
//! (`PLANS/PASEO_WORKSPACES_DESIGN.md`, section 4.5).
//!
//! A workspace is an owner-signed `kind:30621` project with a Paseo marker,
//! which the daemon materializes as `$HOME/workspaces/<directory>`. A
//! session's working directory selects its workspace, and the workspace
//! selects the channel its thread roots post in. A root created in a
//! workspace carries [`WORKSPACE_TAG`], so a replaced daemon places it again.
//! Other working directories post in the legacy `--channel`, when the bridge
//! has one.
//!
//! The parser, the validation rules, and the directory names come from the
//! shared module that `buzz paseo workspace list` uses, so the bridge and
//! the daemon accept the same workspaces.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nostr::{Event, PublicKey};
use serde_json::{json, Value};
use tokio::time::Instant;
use uuid::Uuid;

use super::bridge::BridgeRelay;
use super::frames::Scope;
use super::projects;
use crate::commands::paseo::workspaces::{self as shared, Selection, Workspace};
use crate::error::CliError;

/// Thread root tag naming the workspace (its `d` value) the session started in.
pub(crate) const WORKSPACE_TAG: &str = "paseo-workspace";
/// The daemon directory that holds one directory per workspace.
const WORKSPACES_DIR: &str = "workspaces";
/// Deadline of one workspace lookup, including the channel metadata read.
const LOOKUP_DEADLINE: Duration = Duration::from_secs(5);
/// How long listing and placement reuse a lookup, aged from its start.
const REUSE: Duration = Duration::from_secs(30);
/// Events per page of a workspace read.
const PAGE: usize = 100;
/// Channels per metadata read, as `workspaces::load` chunks them.
const METADATA_CHUNK: usize = 256;

/// Where a new thread root posts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Route {
    pub channel: Uuid,
    /// The workspace's `d` value, recorded on the root as [`WORKSPACE_TAG`].
    pub workspace: Option<String>,
}

/// The `$HOME/workspaces/<directory>` that a client working directory lies
/// in, after lexical normalization and without resolving links.
/// `$HOME/workspaces` itself is in no workspace, and neither is a shared
/// clone under `$HOME/code` that a workspace names as a source.
pub(crate) fn directory_of_cwd(home: &Path, cwd: &str) -> Option<String> {
    let cwd = projects::normalise(Path::new(cwd))?;
    let rest = cwd.strip_prefix(home.join(WORKSPACES_DIR)).ok()?;
    match rest.components().next()? {
        std::path::Component::Normal(name) => name.to_str().map(str::to_owned),
        _ => None,
    }
}

/// The daemon path of a workspace directory name.
pub(crate) fn path_of(home: &Path, directory: &str) -> PathBuf {
    home.join(WORKSPACES_DIR).join(directory)
}

/// The route of a new thread root when it needs no relay read: a working
/// directory outside every workspace posts in the legacy channel. `None`
/// means [`resolve`] must decide.
pub(crate) fn immediate(scope: &Scope, cwd: Option<&str>) -> Option<Route> {
    let in_workspace = cwd.is_some_and(|cwd| directory_of_cwd(&scope.home, cwd).is_some());
    let channel = scope.channel.filter(|_| !in_workspace)?;
    Some(Route {
        channel,
        workspace: None,
    })
}

/// The route of a new thread root from the session's working directory,
/// with a fresh workspace lookup. Errors are retryable or name the fix, and
/// a workspace session never falls back to the legacy channel.
pub(crate) async fn resolve<R: BridgeRelay>(
    relay: &R,
    scope: &Scope,
    cache: &Cache,
    cwd: Option<&str>,
) -> Result<Route, CliError> {
    if let Some(route) = immediate(scope, cwd) {
        return Ok(route);
    }
    let Some(directory) = cwd.and_then(|cwd| directory_of_cwd(&scope.home, cwd)) else {
        return Err(no_channel(relay, scope, cache, cwd).await);
    };
    let selection = cache.fresh(relay, scope.owner).await?;
    let Some(workspace) = selection
        .workspaces
        .iter()
        .find(|workspace| workspace.directory == directory)
    else {
        return Err(CliError::Other(format!(
            "{} is not an active Paseo workspace: it was deleted, replaced, or skipped as invalid (see `buzz paseo workspace list`). Nothing was posted.",
            path_of(&scope.home, &directory).display()
        )));
    };
    require_members(relay, scope, workspace).await?;
    Ok(Route {
        channel: workspace.channel,
        workspace: Some(workspace.slug.clone()),
    })
}

/// The error for a working directory outside every workspace when the
/// bridge has no legacy channel. It names the workspace directories whose
/// sources include the directory's GitHub project, when a recent lookup has
/// them.
async fn no_channel<R: BridgeRelay>(
    relay: &R,
    scope: &Scope,
    cache: &Cache,
    cwd: Option<&str>,
) -> CliError {
    let shown = cwd.unwrap_or("this directory");
    let mut message = format!(
        "{shown} is not in a Paseo workspace, and this provider has no legacy --channel, so nothing was posted. Create a workspace with `buzz paseo workspace create`"
    );
    let project = cwd.and_then(|cwd| projects::project_of_cwd(&scope.home, cwd));
    if let Some(project) = project {
        let linked: Vec<String> = match cache.recent(relay, scope.owner).await {
            Ok(selection) => selection
                .workspaces
                .iter()
                .filter(|workspace| {
                    workspace.sources.iter().any(|source| {
                        source
                            .strip_prefix("https://github.com/")
                            .is_some_and(|repo| repo.eq_ignore_ascii_case(&project))
                    })
                })
                .map(|workspace| {
                    path_of(&scope.home, &workspace.directory)
                        .display()
                        .to_string()
                })
                .collect(),
            Err(error) => {
                eprintln!("acp-bridge: workspace lookup for {project} failed: {error}");
                Vec::new()
            }
        };
        if !linked.is_empty() {
            message.push_str(&format!(
                ", or start the session in a workspace whose sources include {project}: {}",
                linked.join(", ")
            ));
        }
    }
    message.push('.');
    CliError::Other(message)
}

/// Refuse a workspace whose channel the owner, the bridge signer, or the
/// agent cannot post in, from one strong read of the relay-signed roster.
async fn require_members<R: BridgeRelay>(
    relay: &R,
    scope: &Scope,
    workspace: &Workspace,
) -> Result<(), CliError> {
    let channel = workspace.channel;
    let lookup = async {
        let relay_key = relay.relay_identity().await?;
        let rosters = relay
            .query(json!({
                "kinds": [buzz_core::kind::KIND_NIP29_GROUP_MEMBERS],
                "authors": [relay_key.to_hex()],
                "#d": [channel.to_string()],
                "limit": 2,
                "consistency": "strong",
            }))
            .await?;
        if rosters.len() > 1 {
            return Err(CliError::Other(format!(
                "ambiguous membership of channel {channel}; retry"
            )));
        }
        Ok((relay_key, rosters.into_iter().next()))
    };
    let (relay_key, roster) = tokio::time::timeout(LOOKUP_DEADLINE, lookup)
        .await
        .unwrap_or_else(|_| {
            Err(CliError::Other(format!(
                "membership lookup of channel {channel} timed out; retry"
            )))
        })?;
    let member = |key: PublicKey| {
        roster.as_ref().is_some_and(|roster| {
            buzz_sdk::gateway::channel_member(roster, relay_key, channel, key)
        })
    };
    let mut missing = Vec::new();
    if !member(scope.owner) {
        missing.push("you (the owner)");
    }
    if scope.signer.public_key() != scope.owner && !member(scope.signer.public_key()) {
        missing.push("the Paseo gateway");
    }
    if !member(scope.agent) {
        missing.push("the agent");
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(CliError::Other(format!(
        "workspace {} posts in channel {channel}, where {} cannot post. Add the missing members (`buzz paseo workspace create` adds the gateway), then retry. Nothing was posted.",
        workspace.slug,
        missing.join(" and ")
    )))
}

/// The workspace slug recorded on a thread root, when it carries exactly
/// one non-empty [`WORKSPACE_TAG`] and one of the `trusted` keys (the owner
/// and the bridge's signer) authored it. Other channel members cannot
/// choose a directory.
pub(crate) fn slug_of_root(root: &Event, trusted: &[PublicKey]) -> Option<String> {
    if !trusted.contains(&root.pubkey) {
        return None;
    }
    let mut values = root.tags.iter().filter_map(|tag| {
        let parts = tag.as_slice();
        (parts.first().map(String::as_str) == Some(WORKSPACE_TAG)).then(|| parts.get(1))
    });
    let value = values.next()??;
    let valid = !value.is_empty() && value.len() <= buzz_sdk::PROJECT_D_MAX_LEN;
    (values.next().is_none() && valid).then(|| value.clone())
}

/// Placement step 3: the directory of the live workspace that binds the
/// root's channel, in the shared-channel order (newest `created_at`, then
/// lowest `d`), when that directory exists.
pub(crate) fn bound_directory(home: &Path, root: &Event, live: &Selection) -> Option<PathBuf> {
    let channel = root.tags.iter().find_map(|tag| match tag.as_slice() {
        [name, value, ..] if name == "h" => Uuid::parse_str(value).ok(),
        _ => None,
    })?;
    let workspace = live
        .workspaces
        .iter()
        .filter(|workspace| workspace.channel == channel)
        .min_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.slug.cmp(&b.slug))
        })?;
    Some(path_of(home, &workspace.directory)).filter(|dir| dir.is_dir())
}

/// Placement step 1: the directory of a root's recorded workspace, live or
/// retained, when it exists.
pub(crate) fn recorded_directory(home: &Path, slug: &str) -> Option<PathBuf> {
    Some(path_of(home, &shared::directory_name(slug))).filter(|dir| dir.is_dir())
}

/// The live workspaces of one listing, read at most once, from a lookup at
/// most [`REUSE`] old.
pub(crate) struct Live<'a, R> {
    relay: &'a R,
    owner: PublicKey,
    cache: &'a Cache,
    selection: Option<Arc<Selection>>,
}

impl<'a, R: BridgeRelay> Live<'a, R> {
    pub(crate) fn new(relay: &'a R, owner: PublicKey, cache: &'a Cache) -> Self {
        Self {
            relay,
            owner,
            cache,
            selection: None,
        }
    }

    pub(crate) async fn get(&mut self) -> Result<Arc<Selection>, CliError> {
        if let Some(selection) = &self.selection {
            return Ok(Arc::clone(selection));
        }
        let selection = self.cache.recent(self.relay, self.owner).await?;
        self.selection = Some(Arc::clone(&selection));
        Ok(selection)
    }
}

/// Workspace lookups shared by session creation, `/fresh`, and listing.
/// Session creation and `/fresh` always read the relay; listing reuses a
/// result for [`REUSE`]. A lookup that started before the stored one never
/// replaces it.
#[derive(Default)]
pub(crate) struct Cache {
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    started: u64,
    stored: Option<Stored>,
}

struct Stored {
    ticket: u64,
    at: Instant,
    selection: Arc<Selection>,
}

impl Cache {
    fn state(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Read the owner's workspaces from the relay and remember them.
    pub(crate) async fn fresh<R: BridgeRelay>(
        &self,
        relay: &R,
        owner: PublicKey,
    ) -> Result<Arc<Selection>, CliError> {
        let (ticket, at) = {
            let mut state = self.state();
            state.started += 1;
            (state.started, Instant::now())
        };
        let selection = Arc::new(read(relay, owner).await?);
        for skipped in &selection.skipped {
            eprintln!(
                "acp-bridge: skipped Paseo workspace {}: {}",
                skipped.slug, skipped.reason
            );
        }
        let mut state = self.state();
        if state
            .stored
            .as_ref()
            .is_none_or(|stored| stored.ticket < ticket)
        {
            state.stored = Some(Stored {
                ticket,
                at,
                selection: Arc::clone(&selection),
            });
        }
        Ok(selection)
    }

    /// A lookup at most [`REUSE`] old, or a fresh one.
    pub(crate) async fn recent<R: BridgeRelay>(
        &self,
        relay: &R,
        owner: PublicKey,
    ) -> Result<Arc<Selection>, CliError> {
        let reused = self
            .state()
            .stored
            .as_ref()
            .filter(|stored| stored.at.elapsed() <= REUSE)
            .map(|stored| Arc::clone(&stored.selection));
        match reused {
            Some(selection) => Ok(selection),
            None => self.fresh(relay, owner).await,
        }
    }
}

/// One workspace lookup, the reads of `workspaces::load` through the
/// bridge's relay: the owner's projects, then the metadata of the channels
/// they name, both from the writer.
async fn read<R: BridgeRelay>(relay: &R, owner: PublicKey) -> Result<Selection, CliError> {
    let lookup = async {
        let projects = read_all(
            relay,
            json!({"kinds": [buzz_core::kind::KIND_PROJECT], "authors": [owner.to_hex()]}),
        )
        .await?;
        let channels: std::collections::BTreeSet<String> =
            projects.iter().filter_map(channel_tag).collect();
        let channels: Vec<String> = channels.into_iter().collect();
        let mut metadata = Vec::new();
        for chunk in channels.chunks(METADATA_CHUNK) {
            let filter =
                json!({"kinds": [buzz_core::kind::KIND_NIP29_GROUP_METADATA], "#d": chunk});
            for event in read_all(relay, filter).await? {
                metadata.push(serde_json::to_value(event).map_err(|error| {
                    CliError::Other(format!("invalid channel metadata: {error}"))
                })?);
            }
        }
        Ok(shared::select(owner, &projects, &metadata))
    };
    tokio::time::timeout(LOOKUP_DEADLINE, lookup)
        .await
        .unwrap_or_else(|_| {
            Err(CliError::Other(format!(
                "Paseo workspace lookup did not finish within {}s; retry",
                LOOKUP_DEADLINE.as_secs()
            )))
        })
        .map_err(|error| CliError::Other(format!("Paseo workspace lookup failed: {error}")))
}

/// The channel a project names, as `workspaces::load` reads it.
fn channel_tag(event: &Event) -> Option<String> {
    let value = event.tags.iter().find_map(|tag| match tag.as_slice() {
        [name, value, ..] if name == "buzz-channel" => Some(value),
        _ => None,
    })?;
    Uuid::parse_str(value)
        .ok()
        .map(|channel| channel.to_string())
}

/// Every event of `filter`, in pages, failing above
/// [`shared::READ_BOUND`] rather than treating a truncated read as complete.
async fn read_all<R: BridgeRelay>(relay: &R, mut filter: Value) -> Result<Vec<Event>, CliError> {
    filter["limit"] = json!(PAGE);
    filter["consistency"] = json!("strong");
    let mut events: Vec<Event> = Vec::new();
    loop {
        let page = relay.query(filter.clone()).await?;
        if page.len() > PAGE {
            return Err(CliError::Other("relay exceeded the page bound".into()));
        }
        let full = page.len() == PAGE;
        let last = page
            .iter()
            .min_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| b.id.cmp(&a.id))
            })
            .map(|event| (event.created_at.as_secs(), event.id.to_hex()));
        events.extend(page);
        if events.len() > shared::READ_BOUND as usize {
            return Err(CliError::Other(format!(
                "more than {} events; the read would be incomplete",
                shared::READ_BOUND
            )));
        }
        let Some((until, before_id)) = last.filter(|_| full) else {
            events.sort_by_key(|event| event.id);
            events.dedup_by_key(|event| event.id);
            return Ok(events);
        };
        if filter["until"] == json!(until) && filter["before_id"] == json!(before_id) {
            return Err(CliError::Other(
                "relay did not advance the read cursor".into(),
            ));
        }
        filter["until"] = json!(until);
        filter["before_id"] = json!(before_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_directories_map_to_workspace_directories() {
        let home = Path::new("/home/paseo");
        for (cwd, want) in [
            ("/home/paseo/workspaces/payments", Some("payments")),
            ("/home/paseo/workspaces/payments/", Some("payments")),
            // A subdirectory the user made stays in the workspace.
            (
                "/home/paseo/workspaces/payments/notes/src",
                Some("payments"),
            ),
            ("/home/paseo/workspaces/a/../payments", Some("payments")),
            ("/home/paseo/workspaces/./payments", Some("payments")),
            (
                "/home/paseo/workspaces/payments/notes/../../../code/a/b",
                None,
            ),
            ("/home/paseo/workspaces/payments/..", None),
            ("/home/paseo/workspaces", None),
            (
                "/home/paseo/workspaces/_0123456789abcdef",
                Some("_0123456789abcdef"),
            ),
            ("/home/paseo/code/voltagecloud/buzz", None),
            ("/home/paseo/buzz", None),
            ("/home/other/workspaces/payments", None),
            ("workspaces/payments", None),
        ] {
            assert_eq!(directory_of_cwd(home, cwd).as_deref(), want, "{cwd}");
        }
    }
}
