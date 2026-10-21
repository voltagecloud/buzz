//! What the selected community keeps beside messages, as the user sees
//! it: an agent's memories (buzz-app's `agentMemories`), a channel's
//! Canvas (`canvas`), its details (`channelDetails`), the user's kit of
//! teams, templates and sidebar groups (`channelKit`), and the
//! community's repositories and projects (`projects`, `projectGit`).
//! Saving a Canvas needs `send-messages`; saving details or the kit,
//! `manage-channels`.

use super::capabilities::{Failure, channel, community, on_tokio};
use super::protocol::INVALID_PARAMS;
use buzz_relay_client::EventId;
use buzz_relay_client::canvas::{HistoryCursor, Revision};
use buzz_relay_client::kit::KitValue;
use buzz_relay_client::lifecycle::{AttemptStatus, DetailsDraft};
use buzz_relay_client::projects::{
    Entity as ProjectEntity, EntityFailure, EntityKind, EntityRoute, GitRead, Home,
};
use gpui_kit::*;
use serde_json::{Value, json};

fn invalid(message: impl Into<String>) -> Failure {
    (INVALID_PARAMS, message.into())
}

fn text<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

/// An optional event id at `key`.
fn event_id(params: &Value, key: &str) -> Result<Option<EventId>, Failure> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .and_then(|hex| EventId::from_hex(hex).ok())
            .map(Some)
            .ok_or_else(|| invalid(format!("{key} is not an event id"))),
    }
}

/// `agents/memories`: what one of the user's agents remembers, newest
/// per slug. Only an agent's owner reads them.
pub fn memories(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    // As given: `memoryAgent` takes lowercase hex only.
    let agent = text(params, "agent")
        .ok_or_else(|| invalid("agent is required"))?
        .to_owned();
    let session = community(cx)?.read(cx).session.clone();
    // Any agent key but yours; the relay answers only its owner, so an
    // agent listed elsewhere reads too (`memoryAgent`).
    if !buzz_relay_client::session::memory_agent(&agent, session.viewer()) {
        return Err(invalid("agent is not an agent key"));
    }
    // Memories are read only while the live connection is up, before and
    // after the read (`memoryConnected`).
    let unavailable = || (2, "Memory read unavailable".to_owned());
    if !session.snapshot().borrow().live {
        return Err(unavailable());
    }
    Ok(on_tokio(cx, async move {
        use buzz_relay_client::session::MemoryError;
        let listing = session
            .agent_memories(&agent)
            .await
            .map_err(|error| match error {
                // Not yours to read (`denied`), apart from a read that failed.
                MemoryError::Denied => (1, "denied".to_owned()),
                MemoryError::Failed(error) => (2, error),
            })?;
        if !session.snapshot().borrow().live {
            return Err(unavailable());
        }
        Ok(json!({
            "entries": listing.entries.iter().map(|entry| json!({
                "slug": entry.slug,
                "body": entry.body,
                "eventId": entry.event_id,
                "createdAt": entry.created_at,
            })).collect::<Vec<_>>(),
            "partial": listing.partial,
        }))
    }))
}

fn revision(revision: &Revision) -> Value {
    json!({
        "id": revision.id.to_hex(),
        "author": revision.author,
        "createdAt": revision.created_at,
        "content": revision.content,
        // The whole signed event, as buzz-app's revisions are.
        "kind": revision.kind,
        "tags": revision.tags,
        "sig": revision.sig,
    })
}

/// `canvas/read`: a channel's Canvas now, or `null` without one;
/// `strong` reads past replicas, as before an edit.
pub fn canvas_read(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let strong = params
        .get("strong")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let head = session
            .canvas_read(&channel, strong)
            .await
            .map_err(|error| (2, error.to_string()))?;
        Ok(head.as_ref().map_or(Value::Null, revision))
    }))
}

/// `canvas/available`: whether this connection can save a Canvas
/// (`canvas.available`).
pub fn canvas_available(cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        Ok(Value::Bool(session.canvas_available().await))
    }))
}

/// `canvas/history`: a channel's Canvas revisions, newest first, 25 at a
/// time; `cursor` is the `next` of the page before.
pub fn canvas_history(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let cursor = match params.get("cursor").filter(|cursor| !cursor.is_null()) {
        None => None,
        Some(cursor) => Some(HistoryCursor {
            until: cursor
                .get("until")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("cursor.until is required"))?,
            before_id: event_id(cursor, "beforeId")?
                .ok_or_else(|| invalid("cursor.beforeId is required"))?,
        }),
    };
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let page = session
            .canvas_history(&channel, cursor)
            .await
            .map_err(|error| (2, error.to_string()))?;
        Ok(json!({
            "revisions": page.revisions.iter().map(revision).collect::<Vec<_>>(),
            "next": page.next.map(|next| json!({
                "until": next.until,
                "beforeId": next.before_id.to_hex(),
            })),
        }))
    }))
}

/// `canvas/save`: replaces a channel's Canvas, if it is still `expected`
/// (the id read last, or `null` for none). A conflict fails with code 4.
pub fn canvas_save(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let content = text(params, "content")
        .ok_or_else(|| invalid("content is required"))?
        .to_owned();
    if buzz_relay_client::canvas::too_large(&content) {
        return Err(invalid("content is too large"));
    }
    let expected = event_id(params, "expected")?;
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        match session.canvas_save(&channel, &content, expected).await {
            Ok(saved) => Ok(revision(&saved)),
            // The Canvas changed since `expected` (code 5, not 4).
            Err(error) if error.is_conflict() => Err((5, error.to_string())),
            Err(error) => Err((3, error.to_string())),
        }
    }))
}

fn draft(draft: &DetailsDraft) -> Value {
    json!({
        "name": draft.name,
        "description": draft.description,
        "private": draft.private,
        "ttlSeconds": draft.ttl,
        "labels": draft.labels,
    })
}

/// `channels/details`: a channel's name, description, visibility,
/// lifetime and labels, the `version` a save starts from, whether the
/// user may edit them, and a save still in flight or unconfirmed.
pub fn details(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let session = community.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let details = session
            .channel_details(&channel)
            .await
            .map_err(|error| (2, error))?;
        let attempt = session.details_attempt(&channel);
        Ok(json!({
            "channel": details.channel,
            "version": details.version.to_hex(),
            "details": draft(&details.draft),
            "canEdit": details.can_edit,
            "attempt": attempt.map(|attempt| json!({
                "details": draft(&attempt.draft),
                "status": match attempt.status {
                    AttemptStatus::Saving => "saving",
                    AttemptStatus::Unconfirmed => "unconfirmed",
                },
            })),
        }))
    }))
}

/// `channels/saveDetails`: saves `details` over `version`; with
/// `check: true`, checks an unconfirmed save instead.
pub fn save_details(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    let channel = channel(params, community.read(cx))?;
    let session = community.read(cx).session.clone();
    if params.get("check").and_then(Value::as_bool) == Some(true) {
        return Ok(on_tokio(cx, async move {
            session
                .check_channel_details(&channel)
                .await
                .map(|()| Value::Null)
                .map_err(|error| (3, error))
        }));
    }
    let version = event_id(params, "version")?.ok_or_else(|| invalid("version is required"))?;
    let given = params
        .get("details")
        .ok_or_else(|| invalid("details is required"))?;
    // As given: the save refuses a name with spaces around it.
    let name = text(given, "name").ok_or_else(|| invalid("details.name is required"))?;
    let labels = match given.get("labels") {
        None | Some(Value::Null) => None,
        Some(labels) => Some(
            labels
                .as_array()
                .ok_or_else(|| invalid("details.labels must be a list"))?
                .iter()
                .map(|label| {
                    label
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("details.labels must be text"))
                })
                .collect::<Result<_, _>>()?,
        ),
    };
    let ttl = super::capabilities::ttl_seconds(
        given,
        "ttlSeconds",
        "Channel duration could not be verified.",
    )?;
    // The whole draft, as buzz-app validates it: a missing field would
    // clear the description or make a private channel public.
    let draft = DetailsDraft {
        name: name.to_owned(),
        description: text(given, "description")
            .ok_or_else(|| invalid("details.description is required (empty clears it)"))?
            .to_owned(),
        private: given
            .get("private")
            .and_then(Value::as_bool)
            .ok_or_else(|| invalid("details.private is required"))?,
        ttl,
        labels,
    };
    Ok(on_tokio(cx, async move {
        session
            .save_channel_details(&channel, version, draft)
            .await
            .map(|()| Value::Null)
            .map_err(|error| (3, error))
    }))
}

/// `kit/get`: the user's kit in the selected community (teams, channel
/// templates and sidebar groups), read once if it never was.
pub fn kit(cx: &App) -> Result<Value, Failure> {
    let session = community(cx)?.read(cx).session.clone();
    session.kit_ensure();
    let snapshot = session.kit().borrow().clone();
    Ok(json!({
        "status": format!("{:?}", snapshot.status).to_ascii_lowercase(),
        "entries": snapshot.entries.iter().map(|entry| json!({
            "eventId": entry.event_id.to_hex(),
            "createdAt": entry.created_at,
            "version": entry.record.version,
            "deleted": entry.record.deleted,
            "value": serde_json::to_value(&entry.record.value).unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
        "error": snapshot.error,
    }))
}

/// `kit/save`: saves one kit `value` (a `team`, `template` or `groups`,
/// tagged by `type`) over `expected`, the event id it was read as, or
/// deletes it with `deleted: true`. Answers with the new event id.
pub fn save_kit(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let value: KitValue = params
        .get("value")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(|| invalid("value is not a kit value"))?;
    let expected = event_id(params, "expected")?;
    let deleted = params
        .get("deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        session
            .kit_save(value, expected, deleted)
            .await
            .map(|id| json!({ "eventId": id.to_hex() }))
            .map_err(|error| (3, error))
    }))
}

fn failure(failure: EntityFailure) -> Failure {
    let text = match failure {
        EntityFailure::NotFound => "notFound",
        EntityFailure::Denied => "denied",
        EntityFailure::Unavailable => "unavailable",
        EntityFailure::InvalidTarget => "invalidTarget",
    };
    // A read that failed (code 2), saying why.
    (2, text.to_owned())
}

/// A repository or project. `route` is what `ui/navigate` takes as the
/// Projects page's `params`.
fn entity(entity: &ProjectEntity) -> Value {
    json!({
        "kind": match entity.kind {
            EntityKind::Repo => "repo",
            EntityKind::Project => "project",
        },
        "owner": entity.owner,
        "dtag": entity.dtag,
        "address": entity.address,
        "name": entity.name,
        "description": entity.description,
        "route": entity.route().map(|route| route.params()),
        "event": serde_json::to_value(&*entity.event).unwrap_or(Value::Null),
    })
}

fn events(events: &[buzz_relay_client::event::Verified]) -> Vec<Value> {
    events
        .iter()
        .map(|event| serde_json::to_value(&**event).unwrap_or(Value::Null))
        .collect()
}

/// `projects/list`: the community's repositories and projects.
pub fn projects(cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let projects = community(cx)?.read(cx).session.projects();
    Ok(on_tokio(cx, async move {
        let listed = projects.list().await.map_err(failure)?;
        Ok(json!(listed.iter().map(entity).collect::<Vec<_>>()))
    }))
}

/// `projects/load`: everything a Projects route shows except repository
/// content: the entity, a project's repositories, its channels, and the
/// issue or pull request the route names with its comments.
pub fn project(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let route = params
        .get("route")
        .and_then(EntityRoute::from_params)
        .ok_or_else(|| invalid("route is not a Projects route"))?;
    let projects = community(cx)?.read(cx).session.projects();
    Ok(on_tokio(cx, async move {
        let detail = projects.load(route).await.map_err(failure)?;
        Ok(json!({
            "entity": entity(&detail.entity),
            "repositories": detail.repositories.iter().map(entity).collect::<Vec<_>>(),
            "unavailableRepositories": detail.unavailable_repositories,
            "channels": detail.channels,
            "item": detail.item.as_ref().map(|item| serde_json::to_value(&**item).unwrap_or(Value::Null)),
            "status": detail.status,
            "commit": detail.commit,
            "activity": events(&detail.activity),
            "items": events(&detail.items),
            "truncated": detail.truncated,
        }))
    }))
}

/// `projects/home`: a channel's project, `{"home": null}` without one,
/// or `{"ambiguous": true}` when it has several.
pub fn project_home(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let community = community(cx)?;
    // Any channel id, such as an open one you have not joined.
    let channel = super::capabilities::readable(params, community.read(cx))?;
    let projects = community.read(cx).session.projects();
    Ok(on_tokio(cx, async move {
        Ok(match projects.home(channel).await.map_err(failure)? {
            Home::None => json!({ "home": null, "ambiguous": false }),
            Home::Ambiguous => json!({ "home": null, "ambiguous": true }),
            Home::Project(project) => json!({ "home": entity(&project), "ambiguous": false }),
        })
    }))
}

/// `projects/git`: a repository's head, latest commits, files and
/// README, and the file at `path` or the diff of `commit`, read with Git.
pub fn git(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let read = GitRead {
        owner: text(params, "owner")
            .ok_or_else(|| invalid("owner is required"))?
            .to_ascii_lowercase(),
        dtag: text(params, "dtag")
            .ok_or_else(|| invalid("dtag is required"))?
            .to_owned(),
        commit: text(params, "commit").map(str::to_ascii_lowercase),
        path: text(params, "path").map(str::to_owned),
    }
    .parse()
    .ok_or_else(|| invalid("not a repository, commit or path Git can read"))?;
    let projects = community(cx)?.read(cx).session.projects();
    Ok(on_tokio(cx, async move {
        let snapshot = projects.read_git(read).await.map_err(failure)?;
        Ok(json!({
            "head": snapshot.head,
            "commits": snapshot.commits.iter().map(|commit| json!({
                "hash": commit.hash,
                "author": commit.author,
                "date": commit.date,
                "subject": commit.subject,
            })).collect::<Vec<_>>(),
            "files": snapshot.files.iter().map(|file| json!({
                "path": file.path,
                "hash": file.hash,
                "size": file.size,
            })).collect::<Vec<_>>(),
            "readme": snapshot.readme,
            "file": snapshot.file.map(|file| json!({
                "path": file.path,
                "content": file.content,
                "size": file.size,
            })),
            "diff": snapshot.diff,
        }))
    }))
}
