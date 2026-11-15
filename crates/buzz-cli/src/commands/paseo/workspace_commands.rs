//! Owner-key workspace writes and the daemon's validated JSON listing.
use buzz_core::kind::KIND_PROJECT;
use nostr::{Event, EventBuilder, PublicKey, Tag, Timestamp};
use serde_json::{json, Value};
use uuid::Uuid;

use super::workspaces;
use crate::{client::BuzzClient, commands::parse_write_response, error::CliError};

#[derive(clap::Subcommand)]
pub(crate) enum WorkspaceCmd {
    /// Create a workspace; metadata is readable by the whole community.
    Create {
        /// Paseo project-list name; derived from --name when omitted.
        /// Explicit values use lowercase letters, digits, dots, underscores, hyphens.
        #[arg(long, required_unless_present = "name")]
        slug: Option<String>,
        /// Original display metadata, stored verbatim; also supplies the default slug.
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        channel: Uuid,
        /// GitHub URL for prompt context; repeatable. Sources are not materialized locally.
        #[arg(long)]
        source: Vec<String>,
        /// Optional NIP-34 member coordinate, or your repository ID.
        #[arg(long)]
        repo: Vec<String>,
        #[arg(long)]
        listed: bool,
        /// Also check this agent's membership; repeatable.
        #[arg(long)]
        agent: Vec<PublicKey>,
    },
    /// Print validated workspaces and directory names as JSON.
    List {
        /// Workspace signer, required when reading through a daemon gateway.
        #[arg(long)]
        owner: Option<PublicKey>,
    },
    /// Update a marked workspace, preserving members and unknown tags.
    Update {
        #[arg(long)]
        slug: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        channel: Option<Uuid>,
        /// Replace all sources; repeatable.
        #[arg(long, conflicts_with = "clear_sources")]
        source: Vec<String>,
        #[arg(long)]
        clear_sources: bool,
        #[arg(long, conflicts_with = "unlisted")]
        listed: bool,
        #[arg(long)]
        unlisted: bool,
    },
    /// Delete only an owner-signed marked workspace; keep channel memberships.
    Delete {
        #[arg(long)]
        slug: String,
    },
}

pub(super) async fn dispatch(command: WorkspaceCmd, client: &BuzzClient) -> Result<(), CliError> {
    println!("{}", execute(command, client).await?);
    Ok(())
}

async fn execute(command: WorkspaceCmd, client: &BuzzClient) -> Result<Value, CliError> {
    if let WorkspaceCmd::List { owner } = command {
        let selection =
            workspaces::load(client, owner.unwrap_or(client.keys().public_key())).await?;
        for skipped in selection.skipped {
            eprintln!("workspace {:?}: {}", skipped.slug, skipped.reason);
        }
        return serde_json::to_value(selection.workspaces)
            .map_err(|e| CliError::Other(e.to_string()));
    }
    if client.auth_tag_owner_hex().is_some() {
        return Err(CliError::Auth(
            "workspace writes require the owner's direct signing key".into(),
        ));
    }
    match command {
        WorkspaceCmd::Create {
            slug,
            name,
            channel,
            source,
            repo,
            listed,
            agent,
        } => {
            let derived = slug.is_none();
            let slug = match slug {
                Some(slug) => slug,
                None => derive_slug(name.as_deref().unwrap_or_default())?,
            };
            if workspaces::directory_name(&slug) != slug {
                return Err(CliError::Usage(
                    "workspace slug must match ^[a-z0-9][a-z0-9._-]{0,63}$ (for example payments-review); it is displayed unchanged in Paseo".into(),
                ));
            }
            let tags = create_tags(
                client.keys().public_key(),
                &slug,
                name.as_deref(),
                channel,
                &source,
                &repo,
                listed,
            )?;
            let builder = build("", tags.clone())?;
            let candidate = client.sign_event(builder)?;
            validate_candidate(client, &candidate).await?;
            let head = fetch_head(client, &slug).await?;
            if let Some(head) = &head {
                if derived {
                    return Err(CliError::Conflict(format!(
                        "derived workspace slug {slug:?} already exists for this owner; choose an explicit --slug"
                    )));
                }
                require_marker(head)?;
                if head.tags != candidate.tags || head.content != candidate.content {
                    return Err(CliError::Conflict(format!("workspace {slug:?} already exists with different metadata; use workspace update")));
                }
            }
            eprintln!("Warning: workspace metadata (name, channel UUID, and source repository names) is community-readable, even when unlisted.");
            let warnings = super::workspace_membership::ensure(client, channel, &agent).await?;
            let expected_id = head.as_ref().map_or(candidate.id, |head| head.id);
            let mut response = if let Some(head) = head {
                json!({"event_id":head.id,"accepted":true,"unchanged":true})
            } else {
                publish(client, candidate.clone()).await?
            };
            verify_head(client, &slug, expected_id).await?;
            response["slug"] = json!(slug);
            response["message"] = json!(format!(
                "{slug} is the name shown in the Paseo project list"
            ));
            response["warnings"] = json!(warnings);
            if crate::links::is_linkable_dtag(&slug) {
                response["link"] = json!(crate::links::project_link(
                    &client.keys().public_key().to_hex(),
                    &slug
                ));
            }
            Ok(response)
        }
        WorkspaceCmd::Update {
            slug,
            name,
            channel,
            source,
            clear_sources,
            listed,
            unlisted,
        } => {
            if name.is_none()
                && channel.is_none()
                && source.is_empty()
                && !clear_sources
                && !listed
                && !unlisted
            {
                return Err(CliError::Usage(
                    "workspace update requires at least one change".into(),
                ));
            }
            let head = require_head(client, &slug).await?;
            let tags = update_tags(
                &head,
                name.as_deref(),
                channel,
                &source,
                clear_sources,
                listed,
                unlisted,
            )?;
            let candidate = client.sign_event(
                build(&head.content, tags)?.custom_created_at(next_timestamp(&head)?),
            )?;
            validate_candidate(client, &candidate).await?;
            let warnings = if let Some(channel) = channel {
                super::workspace_membership::ensure(client, channel, &[]).await?
            } else {
                Vec::new()
            };
            let mut response = publish(client, candidate.clone()).await?;
            verify_head(client, &slug, candidate.id).await?;
            response["warnings"] = json!(warnings);
            Ok(response)
        }
        WorkspaceCmd::Delete { slug } => {
            let head = require_head(client, &slug).await?;
            let builder = buzz_sdk::build_delete_addressable(
                KIND_PROJECT,
                &client.keys().public_key().to_hex(),
                &slug,
            )
            .map_err(|e| CliError::Usage(e.to_string()))?
            .custom_created_at(next_timestamp(&head)?);
            let response = publish(client, client.sign_event(builder)?).await?;
            if fetch_head(client, &slug).await?.is_some() {
                return Err(CliError::Conflict(
                    "workspace survived deletion; reread and retry".into(),
                ));
            }
            Ok(response)
        }
        WorkspaceCmd::List { .. } => Err(CliError::Other("list already dispatched".into())),
    }
}

// Only ASCII letters are case-folded: Unicode names must not transliterate into ASCII.
fn derive_slug(name: &str) -> Result<String, CliError> {
    let mut slug = String::new();
    let mut separator = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(c.to_ascii_lowercase());
            separator = false;
            if slug.len() >= 64 {
                slug.truncate(64);
                break;
            }
        } else {
            separator = true;
        }
    }
    let slug = slug.trim_end_matches('-').to_owned();
    if slug.is_empty() || workspaces::directory_name(&slug) != slug {
        return Err(CliError::Usage(
            "name cannot produce a readable workspace slug; provide an explicit --slug (for example payments-review)".into(),
        ));
    }
    Ok(slug)
}

fn tag(parts: &[&str]) -> Result<Tag, CliError> {
    Tag::parse(parts.iter().copied()).map_err(|e| CliError::Usage(e.to_string()))
}
fn build(content: &str, tags: Vec<Tag>) -> Result<EventBuilder, CliError> {
    buzz_sdk::build_project_with_tags(content, tags).map_err(|e| CliError::Usage(e.to_string()))
}
fn source_tags(sources: &[String]) -> Result<Vec<Tag>, CliError> {
    if sources.len() > 16 {
        return Err(CliError::Usage("at most 16 sources per workspace".into()));
    }
    sources
        .iter()
        .map(|source| {
            workspaces::validate_source(source).map_err(CliError::Usage)?;
            tag(&["buzz-source", source])
        })
        .collect()
}
#[allow(clippy::too_many_arguments)]
fn create_tags(
    owner: PublicKey,
    slug: &str,
    name: Option<&str>,
    channel: Uuid,
    sources: &[String],
    repos: &[String],
    listed: bool,
) -> Result<Vec<Tag>, CliError> {
    let mut tags = vec![
        tag(&["d", slug])?,
        tag(&["buzz-paseo", "workspace"])?,
        tag(&["buzz-channel", &channel.to_string()])?,
        tag(&[
            "buzz-visibility",
            if listed { "listed" } else { "unlisted" },
        ])?,
    ];
    if let Some(name) = name {
        tags.push(tag(&["name", name])?);
    }
    tags.extend(source_tags(sources)?);
    for repo in repos {
        let coord = if repo.contains(':') {
            repo.clone()
        } else {
            format!("30617:{}:{repo}", owner.to_hex())
        };
        buzz_sdk::ProjectMemberCoord::parse_full(&coord)
            .map_err(|e| CliError::Usage(e.to_string()))?;
        tags.push(tag(&["a", &coord])?);
    }
    Ok(tags)
}
#[allow(clippy::too_many_arguments)]
fn update_tags(
    head: &Event,
    name: Option<&str>,
    channel: Option<Uuid>,
    sources: &[String],
    clear_sources: bool,
    listed: bool,
    unlisted: bool,
) -> Result<Vec<Tag>, CliError> {
    require_marker(head)?;
    let mut tags: Vec<_> = head
        .tags
        .iter()
        .filter(|t| match t.as_slice().first().map(String::as_str) {
            Some("auth") => false,
            Some("name") => name.is_none(),
            Some("buzz-channel") => channel.is_none(),
            Some("buzz-source") => sources.is_empty() && !clear_sources,
            Some("buzz-visibility") => !listed && !unlisted,
            _ => true,
        })
        .cloned()
        .collect();
    if let Some(name) = name {
        tags.push(tag(&["name", name])?);
    }
    if let Some(channel) = channel {
        tags.push(tag(&["buzz-channel", &channel.to_string()])?);
    }
    tags.extend(source_tags(sources)?);
    if listed || unlisted {
        tags.push(tag(&[
            "buzz-visibility",
            if listed { "listed" } else { "unlisted" },
        ])?);
    }
    Ok(tags)
}
fn require_marker(head: &Event) -> Result<(), CliError> {
    if !workspaces::marked(head) {
        return Err(CliError::Usage(
            "project is not a marked Paseo workspace".into(),
        ));
    }
    Ok(())
}
fn next_timestamp(head: &Event) -> Result<Timestamp, CliError> {
    let next = head
        .created_at
        .as_secs()
        .checked_add(1)
        .ok_or_else(|| CliError::Conflict("workspace timestamp cannot advance".into()))?;
    Ok(Timestamp::from(next.max(Timestamp::now().as_secs())))
}
async fn fetch_head(client: &BuzzClient, slug: &str) -> Result<Option<Event>, CliError> {
    if slug.is_empty() || slug.len() > buzz_sdk::PROJECT_D_MAX_LEN {
        return Err(CliError::Usage("invalid workspace slug length".into()));
    }
    let raw=client.query(&json!({"consistency":"strong","kinds":[KIND_PROJECT],"authors":[client.keys().public_key()],"#d":[slug],"limit":1})).await?;
    let mut events: Vec<Event> = serde_json::from_str(&raw)
        .map_err(|e| CliError::Other(format!("invalid project head: {e}")))?;
    if events.len() > 1 {
        return Err(CliError::Other("multiple project heads returned".into()));
    }
    if let Some(head) = events.first() {
        if head.pubkey != client.keys().public_key()
            || head.kind.as_u16() as u32 != KIND_PROJECT
            || head.verify().is_err()
            || head.tags.identifier() != Some(slug)
        {
            return Err(CliError::Other("unrelated or invalid project head".into()));
        }
    }
    Ok(events.pop())
}
async fn require_head(client: &BuzzClient, slug: &str) -> Result<Event, CliError> {
    let head = fetch_head(client, slug)
        .await?
        .ok_or_else(|| CliError::NotFound(format!("workspace {slug:?} not found")))?;
    require_marker(&head)?;
    Ok(head)
}
async fn validate_candidate(client: &BuzzClient, candidate: &Event) -> Result<(), CliError> {
    let channel = candidate
        .tags
        .iter()
        .find_map(|t| match t.as_slice() {
            [k, v, ..] if k == "buzz-channel" => Some(v),
            _ => None,
        })
        .ok_or_else(|| CliError::Usage("workspace needs a channel".into()))?;
    let channel = Uuid::parse_str(channel)
        .map_err(|_| CliError::Usage("invalid workspace channel UUID".into()))?
        .to_string();
    let metadata = client
        .query_all_bounded(
            json!({"consistency":"strong","kinds":[39000],"#d":[channel]}),
            2,
        )
        .await?;
    let selection = workspaces::select(
        client.keys().public_key(),
        std::slice::from_ref(candidate),
        &metadata,
    );
    if selection.workspaces.len() != 1 {
        return Err(CliError::Usage(
            selection
                .skipped
                .first()
                .map(|s| s.reason.clone())
                .unwrap_or("invalid workspace".into()),
        ));
    }
    Ok(())
}
async fn publish(client: &BuzzClient, event: Event) -> Result<Value, CliError> {
    let raw = client.submit_event(event).await?;
    serde_json::from_str(&parse_write_response(
        &raw,
        "workspace changed concurrently; retry",
    )?)
    .map_err(|e| CliError::Other(e.to_string()))
}
async fn verify_head(client: &BuzzClient, slug: &str, id: nostr::EventId) -> Result<(), CliError> {
    let head = fetch_head(client, slug).await?;
    if !head.is_some_and(|head| head.id == id) {
        return Err(CliError::Conflict(
            "workspace changed concurrently; reread and retry".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "workspace_commands_tests.rs"]
mod tests;
